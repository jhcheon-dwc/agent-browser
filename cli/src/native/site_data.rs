//! Site-level Application data: web app manifest, storage usage/quota and
//! "Clear site data" (CDP `Page.getAppManifest`, `Storage.getUsageAndQuota`,
//! `Storage.clearDataForOrigin`).

use serde_json::{json, Value};

use super::actions::DaemonState;
use super::cdp::client::CdpClient;

/// Storage types accepted by `Storage.clearDataForOrigin` (CDP `StorageType`).
pub const STORAGE_TYPES: &[&str] = &[
    "all",
    "appcache",
    "cookies",
    "file_systems",
    "indexeddb",
    "local_storage",
    "shader_cache",
    "websql",
    "service_workers",
    "cache_storage",
    "interest_groups",
    "shared_storage",
    "storage_buckets",
    "other",
];

/// Normalize the `types` parameter of `clear_site_data`: `None`/empty means
/// "all"; otherwise a comma-separated list of CDP storage types, each validated.
pub fn normalize_storage_types(types: Option<&str>) -> Result<String, String> {
    let raw = types.unwrap_or("").trim();
    if raw.is_empty() {
        return Ok("all".to_string());
    }
    let mut out: Vec<&str> = Vec::new();
    for t in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if !STORAGE_TYPES.contains(&t) {
            return Err(format!(
                "Unknown storage type '{}'. Valid types: {}",
                t,
                STORAGE_TYPES.join(", ")
            ));
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.is_empty() {
        return Ok("all".to_string());
    }
    if out.contains(&"all") {
        return Ok("all".to_string());
    }
    Ok(out.join(","))
}

/// Validate that an origin is usable for CDP Storage calls (opaque origins like
/// `about:blank` / `data:` report "null").
pub fn validate_origin(origin: &str) -> Result<&str, String> {
    let o = origin.trim();
    if o.is_empty() || o == "null" || !o.contains("://") {
        return Err(format!(
            "Page has no usable origin ('{}'); navigate to an http(s) page first",
            o
        ));
    }
    Ok(o)
}

/// Resolve the active page's origin via `location.origin`.
pub async fn page_origin(client: &CdpClient, session_id: &str) -> Result<String, String> {
    let result = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({ "expression": "location.origin", "returnByValue": true })),
            Some(session_id),
        )
        .await?;
    let origin = result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    validate_origin(&origin).map(str::to_string)
}

/// Convert a raw `Page.getAppManifest` result into
/// `{url, errors: [string], data: object|null}`.
pub fn parse_manifest_result(raw: &Value) -> Value {
    let url = raw.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let mut errors: Vec<String> = raw
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|e| {
                    let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
                    let critical = e.get("critical").and_then(|c| c.as_i64()).unwrap_or(0);
                    let line = e.get("line").and_then(|l| l.as_i64()).unwrap_or(0);
                    let col = e.get("column").and_then(|c| c.as_i64()).unwrap_or(0);
                    let mut s = String::new();
                    if critical != 0 {
                        s.push_str("[critical] ");
                    }
                    s.push_str(msg);
                    if line > 0 {
                        s.push_str(&format!(" (line {}, column {})", line, col));
                    }
                    s
                })
                .collect()
        })
        .unwrap_or_default();

    let text = raw.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let data = if text.trim().is_empty() {
        Value::Null
    } else {
        match serde_json::from_str::<Value>(text) {
            Ok(v) if v.is_object() => v,
            Ok(_) => {
                errors.push("Manifest is not a JSON object".to_string());
                Value::Null
            }
            Err(e) => {
                errors.push(format!("Manifest JSON parse error: {}", e));
                Value::Null
            }
        }
    };

    json!({ "url": url, "errors": errors, "data": data })
}

pub async fn get_manifest(client: &CdpClient, session_id: &str) -> Result<Value, String> {
    let raw = client
        .send_command("Page.getAppManifest", Some(json!({})), Some(session_id))
        .await?;
    Ok(parse_manifest_result(&raw))
}

pub async fn storage_usage(
    client: &CdpClient,
    session_id: &str,
    origin: &str,
) -> Result<Value, String> {
    let raw = client
        .send_command(
            "Storage.getUsageAndQuota",
            Some(json!({ "origin": origin })),
            Some(session_id),
        )
        .await?;
    let breakdown: Vec<Value> = raw
        .get("usageBreakdown")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|b| {
                    json!({
                        "storageType": b.get("storageType").cloned().unwrap_or(Value::Null),
                        "usage": b.get("usage").and_then(|u| u.as_f64()).unwrap_or(0.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "origin": origin,
        "usage": raw.get("usage").and_then(|u| u.as_f64()).unwrap_or(0.0),
        "quota": raw.get("quota").and_then(|q| q.as_f64()).unwrap_or(0.0),
        "breakdown": breakdown,
    }))
}

pub async fn clear_site_data(
    client: &CdpClient,
    session_id: &str,
    origin: &str,
    storage_types: &str,
) -> Result<Value, String> {
    client
        .send_command(
            "Storage.clearDataForOrigin",
            Some(json!({ "origin": origin, "storageTypes": storage_types })),
            Some(session_id),
        )
        .await?;
    Ok(json!({ "cleared": true, "origin": origin }))
}

// ---------------------------------------------------------------------------
// Daemon action handlers (dispatched from actions.rs)
// ---------------------------------------------------------------------------

/// Origin to operate on: explicit `origin` param, else the active page's.
async fn resolve_origin(
    cmd: &Value,
    client: &CdpClient,
    session_id: &str,
) -> Result<String, String> {
    match cmd.get("origin").and_then(|v| v.as_str()) {
        Some(o) if !o.trim().is_empty() => validate_origin(o).map(str::to_string),
        _ => page_origin(client, session_id).await,
    }
}

pub async fn handle_manifest(state: &DaemonState) -> Result<Value, String> {
    let mgr = state.browser.as_ref().ok_or("Browser not launched")?;
    let session_id = mgr.active_session_id()?.to_string();
    get_manifest(&mgr.client, &session_id).await
}

pub async fn handle_storage_usage(cmd: &Value, state: &DaemonState) -> Result<Value, String> {
    let mgr = state.browser.as_ref().ok_or("Browser not launched")?;
    let session_id = mgr.active_session_id()?.to_string();
    let origin = resolve_origin(cmd, &mgr.client, &session_id).await?;
    storage_usage(&mgr.client, &session_id, &origin).await
}

pub async fn handle_clear_site_data(cmd: &Value, state: &DaemonState) -> Result<Value, String> {
    // Validate before touching the browser so bad input fails fast.
    let types = normalize_storage_types(cmd.get("types").and_then(|v| v.as_str()))?;
    let mgr = state.browser.as_ref().ok_or("Browser not launched")?;
    let session_id = mgr.active_session_id()?.to_string();
    let origin = resolve_origin(cmd, &mgr.client, &session_id).await?;
    clear_site_data(&mgr.client, &session_id, &origin, &types).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_default_to_all() {
        assert_eq!(normalize_storage_types(None).unwrap(), "all");
        assert_eq!(normalize_storage_types(Some("  ")).unwrap(), "all");
        assert_eq!(normalize_storage_types(Some(" , ,")).unwrap(), "all");
    }

    #[test]
    fn types_csv_is_validated_and_deduped() {
        assert_eq!(
            normalize_storage_types(Some("cookies, local_storage,cookies")).unwrap(),
            "cookies,local_storage"
        );
        assert_eq!(
            normalize_storage_types(Some("indexeddb,all")).unwrap(),
            "all"
        );
        let err = normalize_storage_types(Some("cookies,bogus")).unwrap_err();
        assert!(err.contains("bogus"));
    }

    #[test]
    fn origin_validation() {
        assert_eq!(
            validate_origin("http://127.0.0.1:8765").unwrap(),
            "http://127.0.0.1:8765"
        );
        assert!(validate_origin("null").is_err());
        assert!(validate_origin("").is_err());
    }

    #[test]
    fn manifest_parsed() {
        let raw = json!({
            "url": "http://x/manifest.json",
            "errors": [{"message": "icon missing", "critical": 0, "line": 2, "column": 5}],
            "data": "{\"name\":\"한글 앱\",\"display\":\"standalone\"}"
        });
        let v = parse_manifest_result(&raw);
        assert_eq!(v["url"], "http://x/manifest.json");
        assert_eq!(v["data"]["name"], "한글 앱");
        assert_eq!(v["errors"][0], "icon missing (line 2, column 5)");
    }

    #[test]
    fn manifest_missing_is_null_without_errors() {
        let v = parse_manifest_result(&json!({ "url": "", "errors": [] }));
        assert_eq!(v["url"], "");
        assert!(v["data"].is_null());
        assert_eq!(v["errors"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn manifest_invalid_json_reports_error() {
        let v = parse_manifest_result(&json!({
            "url": "http://x/m.json",
            "errors": [{"message": "Line: 1, column: 1, Syntax error.", "critical": 1}],
            "data": "{not json"
        }));
        assert!(v["data"].is_null());
        let errs = v["errors"].as_array().unwrap();
        assert_eq!(errs.len(), 2);
        assert!(errs[0].as_str().unwrap().starts_with("[critical]"));
    }
}
