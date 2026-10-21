//! IndexedDB inspection for the cmux Application devtools tab.
//!
//! Wraps the CDP `IndexedDB` domain. Databases are addressed by the main
//! frame's storage key (`Storage.getStorageKeyForFrame`), falling back to the
//! page's security origin on Chrome builds without storage keys. Record keys
//! and values come back as CDP `RemoteObject`s; they are rendered as compact
//! JSON-ish preview strings capped at [`MAX_PREVIEW_BYTES`].

use futures_util::future::join_all;
use serde_json::{json, Map, Value};

use super::cdp::client::CdpClient;

/// Upper bound (in bytes) for a rendered key/value preview string.
pub const MAX_PREVIEW_BYTES: usize = 2048;
pub const DEFAULT_PAGE_SIZE: i64 = 50;
pub const MAX_PAGE_SIZE: i64 = 1000;

/// Object group Chrome uses for the RemoteObjects returned by `requestData`.
const OBJECT_GROUP: &str = "indexeddb";

/// Serializes `this` in the page; handles the common non-JSON types so a
/// Map/Set/Blob record does not just render as `{}`. Output is pre-capped
/// (without splitting a surrogate pair, which would make the CDP frame
/// unparseable). Cyclic values make JSON.stringify throw; returning
/// `undefined` then selects the preview-based fallback.
const STRINGIFY_FN: &str = r#"function(limit) {
  const rep = (k, v) => {
    if (typeof v === 'bigint') return v.toString() + 'n';
    if (v && typeof v === 'object') {
      if (v instanceof Map) return { '[[Map]]': Array.from(v.entries()) };
      if (v instanceof Set) return { '[[Set]]': Array.from(v.values()) };
      if (v instanceof ArrayBuffer) return 'ArrayBuffer(' + v.byteLength + ')';
      if (v instanceof DataView) return 'DataView(' + v.byteLength + ')';
      if (ArrayBuffer.isView(v)) return v.constructor.name + '(' + v.length + ')';
      if (typeof Blob !== 'undefined' && v instanceof Blob)
        return (v instanceof File ? 'File(' + v.name + ', ' : 'Blob(') + v.size + ' bytes, ' + (v.type || '?') + ')';
    }
    return v;
  };
  let s;
  try { s = JSON.stringify(this, rep); } catch (e) { return undefined; }
  if (typeof s !== 'string') return undefined;
  if (s.length > limit) {
    s = s.slice(0, limit);
    const c = s.charCodeAt(s.length - 1);
    if (c >= 0xD800 && c <= 0xDBFF) s = s.slice(0, -1);
  }
  return typeof s.toWellFormed === 'function' ? s.toWellFormed() : s;
}"#;

/// Where IndexedDB requests are addressed: a storage key or a security origin.
#[derive(Debug, Clone, PartialEq)]
pub struct IdbTarget {
    /// Page origin, reported back to callers.
    pub origin: String,
    pub storage_key: Option<String>,
}

impl IdbTarget {
    /// Base CDP params (`storageKey` or `securityOrigin`) for IndexedDB calls.
    pub fn params(&self) -> Map<String, Value> {
        let mut m = Map::new();
        match &self.storage_key {
            Some(k) => m.insert("storageKey".into(), json!(k)),
            None => m.insert("securityOrigin".into(), json!(self.origin)),
        };
        m
    }
}

/// Paging parameters for `indexeddb_data`, parsed from the daemon command.
#[derive(Debug, Clone, PartialEq)]
pub struct DataParams {
    pub database: String,
    pub store: String,
    pub index: String,
    pub skip: i64,
    pub limit: i64,
}

pub fn parse_data_params(cmd: &Value) -> Result<DataParams, String> {
    let database = required_str(cmd, "database")?;
    let store = required_str(cmd, "store")?;
    let index = cmd
        .get("index")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let skip = int_param(cmd, "skip").unwrap_or(0).max(0);
    let limit = int_param(cmd, "limit")
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    Ok(DataParams {
        database,
        store,
        index,
        skip,
        limit,
    })
}

pub fn required_str(cmd: &Value, key: &str) -> Result<String, String> {
    cmd.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| format!("Missing '{}' parameter", key))
}

/// Accepts numbers or numeric strings (CLI flags arrive either way).
fn int_param(cmd: &Value, key: &str) -> Option<i64> {
    let v = cmd.get(key)?;
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Resolves the storage key / origin of the main frame. An explicit
/// `origin` overrides the page origin (and is addressed by security origin).
pub async fn resolve_target(
    client: &CdpClient,
    session_id: &str,
    origin: Option<&str>,
) -> Result<IdbTarget, String> {
    if let Some(o) = origin.filter(|o| !o.is_empty()) {
        return Ok(IdbTarget {
            origin: o.trim_end_matches('/').to_string(),
            storage_key: None,
        });
    }
    let tree = client
        .send_command_no_params("Page.getFrameTree", Some(session_id))
        .await?;
    let frame = tree
        .get("frameTree")
        .and_then(|t| t.get("frame"))
        .ok_or("Could not determine main frame")?;
    let frame_id = frame.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let origin = frame
        .get("securityOrigin")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if origin.is_empty() || origin == "null" || origin == "://" {
        return Err("The current page has no origin with IndexedDB (open an http(s) page)".into());
    }
    let storage_key = client
        .send_command(
            "Storage.getStorageKeyForFrame",
            Some(json!({ "frameId": frame_id })),
            Some(session_id),
        )
        .await
        .ok()
        .and_then(|r| r.get("storageKey").and_then(|v| v.as_str()).map(String::from))
        .filter(|k| !k.is_empty());
    Ok(IdbTarget {
        origin,
        storage_key,
    })
}

async fn idb_call(
    client: &CdpClient,
    session_id: &str,
    method: &str,
    target: &IdbTarget,
    extra: Value,
) -> Result<Value, String> {
    let mut params = target.params();
    if let Value::Object(extra) = extra {
        params.extend(extra);
    }
    client
        .send_command(method, Some(Value::Object(params)), Some(session_id))
        .await
}

async fn enable(client: &CdpClient, session_id: &str) {
    // Not strictly required by every Chrome build; ignore failures.
    let _ = client
        .send_command_no_params("IndexedDB.enable", Some(session_id))
        .await;
}

/// CDP `KeyPath` (`{type, string?, array?}`) → `null` / string / array.
pub fn key_path_json(kp: Option<&Value>) -> Value {
    let Some(kp) = kp else { return Value::Null };
    match kp.get("type").and_then(|v| v.as_str()) {
        Some("string") => kp.get("string").cloned().unwrap_or(Value::Null),
        Some("array") => kp.get("array").cloned().unwrap_or_else(|| json!([])),
        _ => Value::Null,
    }
}

/// CDP `DatabaseWithObjectStores` → contract shape.
pub fn database_json(db: &Value) -> Value {
    let stores: Vec<Value> = db
        .get("objectStores")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    let indexes: Vec<Value> = s
                        .get("indexes")
                        .and_then(|v| v.as_array())
                        .map(|ix| {
                            ix.iter()
                                .map(|i| {
                                    json!({
                                        "name": i.get("name").cloned().unwrap_or(json!("")),
                                        "keyPath": key_path_json(i.get("keyPath")),
                                        "unique": i.get("unique").and_then(|v| v.as_bool()).unwrap_or(false),
                                        "multiEntry": i.get("multiEntry").and_then(|v| v.as_bool()).unwrap_or(false),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "name": s.get("name").cloned().unwrap_or(json!("")),
                        "keyPath": key_path_json(s.get("keyPath")),
                        "autoIncrement": s.get("autoIncrement").and_then(|v| v.as_bool()).unwrap_or(false),
                        "indexes": indexes,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "name": db.get("name").cloned().unwrap_or(json!("")),
        "version": db.get("version").cloned().unwrap_or(json!(0)),
        "objectStores": stores,
    })
}

pub async fn databases(
    client: &CdpClient,
    session_id: &str,
    origin: Option<&str>,
    only: Option<&str>,
) -> Result<Value, String> {
    let target = resolve_target(client, session_id, origin).await?;
    enable(client, session_id).await;
    let names = idb_call(
        client,
        session_id,
        "IndexedDB.requestDatabaseNames",
        &target,
        json!({}),
    )
    .await?;
    let mut names: Vec<String> = names
        .get("databaseNames")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect())
        .unwrap_or_default();
    // Optional `database` filter: show just that database's schema.
    if let Some(only) = only.filter(|o| !o.is_empty()) {
        names.retain(|n| n == only);
    }
    names.sort();

    let mut dbs = Vec::with_capacity(names.len());
    for name in names {
        let r = idb_call(
            client,
            session_id,
            "IndexedDB.requestDatabase",
            &target,
            json!({ "databaseName": name }),
        )
        .await;
        // One broken/blocked database must not hide the others.
        match r {
            Ok(r) => match r.get("databaseWithObjectStores") {
                Some(db) => dbs.push(database_json(db)),
                None => dbs.push(json!({ "name": name, "version": 0, "objectStores": [] })),
            },
            Err(e) => dbs.push(json!({
                "name": name, "version": 0, "objectStores": [], "error": e,
            })),
        }
    }
    Ok(json!({ "origin": target.origin, "databases": dbs }))
}

pub async fn data(
    client: &CdpClient,
    session_id: &str,
    origin: Option<&str>,
    p: &DataParams,
) -> Result<Value, String> {
    let target = resolve_target(client, session_id, origin).await?;
    enable(client, session_id).await;
    let mut req = json!({
        "databaseName": p.database,
        "objectStoreName": p.store,
        "skipCount": p.skip,
        "pageSize": p.limit,
    });
    // Recent Chrome rejects `indexName: ""` ("Could not get index"); the
    // primary-key walk is selected by omitting it.
    if !p.index.is_empty() {
        req["indexName"] = json!(p.index);
    }
    let r = idb_call(client, session_id, "IndexedDB.requestData", &target, req).await?;

    let empty = Vec::new();
    let arr = r
        .get("objectStoreDataEntries")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    // Render all entries concurrently; on a primary-key walk the primary
    // key equals the key, so it is rendered once.
    let by_index = !p.index.is_empty();
    let entries = join_all(arr.iter().map(|e| async move {
        let key = render_object(client, session_id, e.get("key")).await;
        let value = render_object(client, session_id, e.get("value")).await;
        let primary_key = if by_index {
            render_object(client, session_id, e.get("primaryKey")).await
        } else {
            key.clone()
        };
        json!({ "key": key, "primaryKey": primary_key, "value": value })
    }))
    .await;
    let _ = client
        .send_command(
            "Runtime.releaseObjectGroup",
            Some(json!({ "objectGroup": OBJECT_GROUP })),
            Some(session_id),
        )
        .await;
    let has_more = r.get("hasMore").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(json!({ "entries": entries, "hasMore": has_more }))
}

pub async fn clear_store(
    client: &CdpClient,
    session_id: &str,
    origin: Option<&str>,
    database: &str,
    store: &str,
) -> Result<Value, String> {
    let target = resolve_target(client, session_id, origin).await?;
    enable(client, session_id).await;
    idb_call(
        client,
        session_id,
        "IndexedDB.clearObjectStore",
        &target,
        json!({ "databaseName": database, "objectStoreName": store }),
    )
    .await?;
    Ok(json!({ "cleared": true }))
}

pub async fn delete_database(
    client: &CdpClient,
    session_id: &str,
    origin: Option<&str>,
    database: &str,
) -> Result<Value, String> {
    let target = resolve_target(client, session_id, origin).await?;
    enable(client, session_id).await;
    idb_call(
        client,
        session_id,
        "IndexedDB.deleteDatabase",
        &target,
        json!({ "databaseName": database }),
    )
    .await?;
    Ok(json!({ "deleted": true }))
}

/// Renders a RemoteObject; objects are JSON-stringified in the page when
/// possible (full fidelity), otherwise rendered from the CDP preview.
async fn render_object(client: &CdpClient, session_id: &str, obj: Option<&Value>) -> String {
    let Some(obj) = obj else {
        return String::new();
    };
    if obj.get("type").and_then(|v| v.as_str()) == Some("object") {
        if let Some(id) = obj.get("objectId").and_then(|v| v.as_str()) {
            let r = client
                .send_command(
                    "Runtime.callFunctionOn",
                    Some(json!({
                        "objectId": id,
                        "functionDeclaration": STRINGIFY_FN,
                        "arguments": [{ "value": MAX_PREVIEW_BYTES + 1 }],
                        "returnByValue": true,
                        "silent": true,
                    })),
                    Some(session_id),
                )
                .await;
            if let Ok(r) = r {
                if r.get("exceptionDetails").is_none() {
                    if let Some(s) = r
                        .get("result")
                        .and_then(|v| v.get("value"))
                        .and_then(|v| v.as_str())
                    {
                        return truncate_preview(s);
                    }
                }
            }
        }
    }
    remote_object_preview(obj)
}

/// Pure fallback rendering of a CDP RemoteObject (uses `value`,
/// `unserializableValue`, `preview` and `description`).
pub fn remote_object_preview(obj: &Value) -> String {
    truncate_preview(&render_remote(obj))
}

fn render_remote(obj: &Value) -> String {
    let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let subtype = obj.get("subtype").and_then(|v| v.as_str());
    if let Some(u) = obj.get("unserializableValue").and_then(|v| v.as_str()) {
        return u.to_string();
    }
    match ty {
        "undefined" => return "undefined".into(),
        "string" | "number" | "boolean" => {
            if let Some(v) = obj.get("value") {
                return v.to_string();
            }
        }
        "object" if subtype == Some("null") => return "null".into(),
        _ => {}
    }
    if let Some(v) = obj.get("value") {
        if !v.is_null() {
            return v.to_string();
        }
    }
    if let Some(p) = obj.get("preview") {
        return render_preview(p);
    }
    obj.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// CDP `ObjectPreview` → `{a: 1, b: "x"}` / `[1, 2]` (… when overflowing).
fn render_preview(p: &Value) -> String {
    let is_array = matches!(
        p.get("subtype").and_then(|v| v.as_str()),
        Some("array") | Some("typedarray")
    );
    let overflow = p.get("overflow").and_then(|v| v.as_bool()).unwrap_or(false);
    let props = p.get("properties").and_then(|v| v.as_array());
    let entries = p.get("entries").and_then(|v| v.as_array());
    if props.map_or(true, |a| a.is_empty()) && entries.map_or(true, |a| a.is_empty()) {
        if let Some(d) = p.get("description").and_then(|v| v.as_str()) {
            if p.get("subtype").and_then(|v| v.as_str()) == Some("date")
                || !matches!(d, "Object" | "Array")
            {
                return d.to_string();
            }
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for prop in props.into_iter().flatten() {
        let val = render_property_value(prop);
        if is_array {
            parts.push(val);
        } else {
            let name = prop.get("name").and_then(|v| v.as_str()).unwrap_or("");
            parts.push(format!("{}: {}", name, val));
        }
    }
    for e in entries.into_iter().flatten() {
        let v = e.get("value").map(render_preview).unwrap_or_default();
        match e.get("key") {
            Some(k) => parts.push(format!("{} => {}", render_preview(k), v)),
            None => parts.push(v),
        }
    }
    if overflow {
        parts.push("…".into());
    }
    if is_array {
        format!("[{}]", parts.join(", "))
    } else {
        format!("{{{}}}", parts.join(", "))
    }
}

fn render_property_value(prop: &Value) -> String {
    if let Some(vp) = prop.get("valuePreview") {
        return render_preview(vp);
    }
    let v = prop.get("value").and_then(|v| v.as_str()).unwrap_or("");
    match prop.get("type").and_then(|v| v.as_str()) {
        Some("string") => serde_json::to_string(v).unwrap_or_default(),
        _ => v.to_string(),
    }
}

/// Caps a preview at [`MAX_PREVIEW_BYTES`] on a UTF-8 char boundary,
/// appending `…` when cut.
pub fn truncate_preview(s: &str) -> String {
    if s.len() <= MAX_PREVIEW_BYTES {
        return s.to_string();
    }
    let ell = "…";
    let mut end = MAX_PREVIEW_BYTES - ell.len();
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &s[..end], ell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate_preview("한글 ok"), "한글 ok");
    }

    #[test]
    fn truncate_cuts_on_char_boundary() {
        let s = "가".repeat(2000); // 6000 bytes
        let t = truncate_preview(&s);
        assert!(t.len() <= MAX_PREVIEW_BYTES);
        assert!(t.ends_with('…'));
        assert!(t.trim_end_matches('…').chars().all(|c| c == '가'));
    }

    #[test]
    fn parse_data_params_defaults_and_clamps() {
        let p = parse_data_params(&json!({ "database": "d", "store": "s" })).unwrap();
        assert_eq!((p.skip, p.limit, p.index.as_str()), (0, 50, ""));
        let p = parse_data_params(
            &json!({ "database": "d", "store": "s", "skip": "5", "limit": 99999, "index": "by" }),
        )
        .unwrap();
        assert_eq!((p.skip, p.limit, p.index.as_str()), (5, MAX_PAGE_SIZE, "by"));
        let p = parse_data_params(&json!({ "database": "d", "store": "s", "skip": -3, "limit": 0 }))
            .unwrap();
        assert_eq!((p.skip, p.limit), (0, 1));
        assert!(parse_data_params(&json!({ "database": "d" })).is_err());
        assert!(parse_data_params(&json!({ "store": "s", "database": "" })).is_err());
    }

    #[test]
    fn target_params_prefer_storage_key() {
        let t = IdbTarget {
            origin: "http://a".into(),
            storage_key: Some("http://a/".into()),
        };
        assert_eq!(Value::Object(t.params()), json!({ "storageKey": "http://a/" }));
        let t = IdbTarget {
            origin: "http://a".into(),
            storage_key: None,
        };
        assert_eq!(Value::Object(t.params()), json!({ "securityOrigin": "http://a" }));
    }

    #[test]
    fn key_paths() {
        assert_eq!(key_path_json(Some(&json!({ "type": "null" }))), Value::Null);
        assert_eq!(
            key_path_json(Some(&json!({ "type": "string", "string": "id" }))),
            json!("id")
        );
        assert_eq!(
            key_path_json(Some(&json!({ "type": "array", "array": ["a", "b"] }))),
            json!(["a", "b"])
        );
        assert_eq!(key_path_json(None), Value::Null);
    }

    #[test]
    fn database_shape() {
        let db = json!({
            "name": "app", "version": 3,
            "objectStores": [{
                "name": "notes", "keyPath": { "type": "null" }, "autoIncrement": true,
                "indexes": [{ "name": "by_t", "keyPath": { "type": "string", "string": "t" },
                              "unique": false, "multiEntry": true }]
            }]
        });
        assert_eq!(
            database_json(&db),
            json!({
                "name": "app", "version": 3,
                "objectStores": [{
                    "name": "notes", "keyPath": null, "autoIncrement": true,
                    "indexes": [{ "name": "by_t", "keyPath": "t", "unique": false, "multiEntry": true }]
                }]
            })
        );
    }

    #[test]
    fn remote_primitives() {
        assert_eq!(remote_object_preview(&json!({ "type": "string", "value": "안녕" })), "\"안녕\"");
        assert_eq!(remote_object_preview(&json!({ "type": "number", "value": 3 })), "3");
        assert_eq!(
            remote_object_preview(&json!({ "type": "number", "unserializableValue": "NaN" })),
            "NaN"
        );
        assert_eq!(remote_object_preview(&json!({ "type": "undefined" })), "undefined");
        assert_eq!(
            remote_object_preview(&json!({ "type": "object", "subtype": "null", "value": null })),
            "null"
        );
    }

    #[test]
    fn remote_object_from_preview() {
        let obj = json!({
            "type": "object", "description": "Object",
            "preview": {
                "type": "object", "description": "Object", "overflow": true,
                "properties": [
                    { "name": "id", "type": "number", "value": "1" },
                    { "name": "t", "type": "string", "value": "한국어" },
                    { "name": "tags", "type": "object", "subtype": "array",
                      "valuePreview": { "type": "object", "subtype": "array", "description": "Array(2)",
                        "overflow": false, "properties": [
                          { "name": "0", "type": "string", "value": "a" },
                          { "name": "1", "type": "number", "value": "2" } ] } }
                ]
            }
        });
        assert_eq!(
            remote_object_preview(&obj),
            r#"{id: 1, t: "한국어", tags: ["a", 2], …}"#
        );
        let date = json!({ "type": "object", "subtype": "date", "description": "Mon Jan 01 2024" });
        assert_eq!(remote_object_preview(&date), "Mon Jan 01 2024");
    }
}
