//! Cache Storage inspection for the cmux devtools Application tab.
//!
//! Wraps CDP `CacheStorage.*`. Caches are addressed by the page's storage key
//! (`Storage.getStorageKey`, falling back to the deprecated
//! `Storage.getStorageKeyForFrame`), or by security origin when no key is
//! available or an explicit `origin` is given.

use serde_json::{json, Value};

use super::cdp::client::CdpClient;

/// Where caches/registrations of the inspected page live. `origin` is empty
/// for opaque pages (about:blank, data:).
pub struct PageStorageTarget {
    pub origin: String,
    pub storage_key: Option<String>,
}

/// Resolve the main frame's security origin and storage key.
pub async fn page_storage_target(
    client: &CdpClient,
    session_id: &str,
) -> Result<PageStorageTarget, String> {
    let tree = client
        .send_command("Page.getFrameTree", None, Some(session_id))
        .await?;
    let frame = &tree["frameTree"]["frame"];
    let frame_id = frame["id"].as_str().unwrap_or_default().to_string();
    let origin = frame["securityOrigin"]
        .as_str()
        .filter(|o| !o.is_empty() && *o != "null")
        .map(str::to_string)
        .or_else(|| frame["url"].as_str().and_then(url_origin))
        .unwrap_or_default();

    let mut storage_key = None;
    if !frame_id.is_empty() {
        for method in ["Storage.getStorageKey", "Storage.getStorageKeyForFrame"] {
            if let Ok(res) = client
                .send_command(method, Some(json!({ "frameId": frame_id })), Some(session_id))
                .await
            {
                if let Some(k) = res["storageKey"].as_str().filter(|k| !k.is_empty()) {
                    storage_key = Some(k.to_string());
                    break;
                }
            }
        }
    }
    Ok(PageStorageTarget {
        origin,
        storage_key,
    })
}

/// `scheme://host[:port]` of a URL, or `None` for opaque URLs (data:, about:).
pub fn url_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    // Drop userinfo; keep host[:port] as written.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    Some(format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

/// `cache_list`: `{origin, caches: [{cacheId, cacheName}]}`.
pub async fn cache_list(
    client: &CdpClient,
    session_id: &str,
    origin: Option<&str>,
) -> Result<Value, String> {
    let (origin, params) = match origin {
        Some(o) => (o.to_string(), json!({ "securityOrigin": o })),
        None => {
            let target = page_storage_target(client, session_id).await?;
            let params = match &target.storage_key {
                Some(k) => json!({ "storageKey": k }),
                None if !target.origin.is_empty() => {
                    json!({ "securityOrigin": target.origin })
                }
                None => return Err("Current page has no origin with Cache Storage".to_string()),
            };
            (target.origin, params)
        }
    };
    let res = client
        .send_command("CacheStorage.requestCacheNames", Some(params), Some(session_id))
        .await?;
    Ok(json!({ "origin": origin, "caches": map_caches(&res) }))
}

fn map_caches(res: &Value) -> Vec<Value> {
    let mut caches: Vec<Value> = res["caches"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| json!({ "cacheId": c["cacheId"], "cacheName": c["cacheName"] }))
                .collect()
        })
        .unwrap_or_default();
    caches.sort_by(|a, b| {
        a["cacheName"]
            .as_str()
            .unwrap_or("")
            .cmp(b["cacheName"].as_str().unwrap_or(""))
    });
    caches
}

/// `cache_entries`: `{entries: [{requestURL, requestMethod, responseStatus,
/// responseType, responseTime}], total}`.
pub async fn cache_entries(
    client: &CdpClient,
    session_id: &str,
    cache_id: &str,
    skip: u64,
    limit: u64,
) -> Result<Value, String> {
    let res = client
        .send_command(
            "CacheStorage.requestEntries",
            Some(json!({ "cacheId": cache_id, "skipCount": skip, "pageSize": limit })),
            Some(session_id),
        )
        .await?;
    Ok(map_entries(&res))
}

fn map_entries(res: &Value) -> Value {
    let entries: Vec<Value> = res["cacheDataEntries"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    json!({
                        "requestURL": e["requestURL"],
                        "requestMethod": e["requestMethod"],
                        "responseStatus": e["responseStatus"],
                        "responseType": e["responseType"],
                        "responseTime": e["responseTime"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // `returnCount` is the total number of entries in the cache (not just this page).
    let total = res["returnCount"]
        .as_u64()
        .or_else(|| res["returnCount"].as_f64().map(|f| f as u64))
        .unwrap_or(entries.len() as u64);
    json!({ "entries": entries, "total": total })
}

/// `cache_delete`: deletes one entry when `request_url` is given, else the cache.
pub async fn cache_delete(
    client: &CdpClient,
    session_id: &str,
    cache_id: &str,
    request_url: Option<&str>,
) -> Result<Value, String> {
    match request_url {
        Some(url) => {
            client
                .send_command(
                    "CacheStorage.deleteEntry",
                    Some(json!({ "cacheId": cache_id, "request": url })),
                    Some(session_id),
                )
                .await?;
        }
        None => {
            client
                .send_command(
                    "CacheStorage.deleteCache",
                    Some(json!({ "cacheId": cache_id })),
                    Some(session_id),
                )
                .await?;
        }
    }
    Ok(json!({ "deleted": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_of_urls() {
        assert_eq!(
            url_origin("http://127.0.0.1:8765/a/b?q#h").as_deref(),
            Some("http://127.0.0.1:8765")
        );
        assert_eq!(
            url_origin("HTTPS://User@Example.COM/x").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(url_origin("https://example.com").as_deref(), Some("https://example.com"));
        assert_eq!(url_origin("data:text/html,hi"), None);
        assert_eq!(url_origin("about:blank"), None);
    }

    #[test]
    fn caches_are_mapped_and_sorted() {
        let res = json!({ "caches": [
            { "cacheId": "b1", "cacheName": "v2", "securityOrigin": "x", "storageKey": "x/" },
            { "cacheId": "a1", "cacheName": "v1", "securityOrigin": "x", "storageKey": "x/" },
        ]});
        let caches = map_caches(&res);
        assert_eq!(caches.len(), 2);
        assert_eq!(caches[0], json!({ "cacheId": "a1", "cacheName": "v1" }));
        assert_eq!(caches[1]["cacheId"], "b1");
        assert!(map_caches(&json!({})).is_empty());
    }

    #[test]
    fn entries_are_mapped_with_total() {
        let res = json!({
            "cacheDataEntries": [{
                "requestURL": "http://h/a", "requestMethod": "GET",
                "requestHeaders": [], "responseTime": 1.5e12,
                "responseStatus": 200, "responseStatusText": "OK",
                "responseType": "basic", "responseHeaders": []
            }],
            "returnCount": 7
        });
        let v = map_entries(&res);
        assert_eq!(v["total"], 7);
        assert_eq!(
            v["entries"][0],
            json!({
                "requestURL": "http://h/a", "requestMethod": "GET",
                "responseStatus": 200, "responseType": "basic", "responseTime": 1.5e12
            })
        );
        let empty = map_entries(&json!({}));
        assert_eq!(empty, json!({ "entries": [], "total": 0 }));
    }
}
