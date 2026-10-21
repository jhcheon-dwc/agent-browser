//! Service worker inspection for the cmux devtools Application tab.
//!
//! Primary path is CDP: `ServiceWorker.enable` makes Chrome replay every known
//! registration/version as `workerRegistrationUpdated` / `workerVersionUpdated`
//! events, which we collect until they go quiet, then `ServiceWorker.disable`.
//! This is preferred over `navigator.serviceWorker.getRegistrations()` because
//! it reports `registrationId` and `runningStatus` (JS cannot), works while the
//! page's main thread is busy or the page is not yet controlled, and
//! `unregister` / `updateRegistration` act on the browser's registration
//! directly. Chrome only accepts those two commands while the domain is
//! enabled, so each one is bracketed by enable/disable.
//!
//! If the ServiceWorker domain is unavailable on the session (e.g. a non-page
//! target or a stripped-down browser), we fall back to the JS API via
//! `Runtime.evaluate` with `awaitPromise`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Value};

use super::cache_storage::{page_storage_target, url_origin};
use super::cdp::client::CdpClient;

/// Give up collecting after this long, even if events keep coming.
const COLLECT_MAX: Duration = Duration::from_millis(2_000);
/// Wait this long for the first replayed event (none arrive without registrations).
const COLLECT_FIRST_IDLE: Duration = Duration::from_millis(500);
/// Stop once no event arrived for this long after the last one.
const COLLECT_IDLE: Duration = Duration::from_millis(250);

#[derive(Default, Debug)]
struct RegState {
    scope_url: String,
    deleted: bool,
    /// versionId -> version payload
    versions: BTreeMap<String, Value>,
}

/// Fold ServiceWorker domain events into the contract's registration list.
/// Pure so it can be unit-tested; `origin` filters by the scope's origin.
fn fold_events(events: &[(String, Value)], origin: Option<&str>) -> Vec<Value> {
    let mut regs: BTreeMap<String, RegState> = BTreeMap::new();
    for (method, params) in events {
        match method.as_str() {
            "ServiceWorker.workerRegistrationUpdated" => {
                for r in params["registrations"].as_array().into_iter().flatten() {
                    let Some(id) = r["registrationId"].as_str() else {
                        continue;
                    };
                    let st = regs.entry(id.to_string()).or_default();
                    if let Some(s) = r["scopeURL"].as_str() {
                        st.scope_url = s.to_string();
                    }
                    st.deleted = r["isDeleted"].as_bool().unwrap_or(false);
                }
            }
            "ServiceWorker.workerVersionUpdated" => {
                for v in params["versions"].as_array().into_iter().flatten() {
                    let (Some(rid), Some(vid)) =
                        (v["registrationId"].as_str(), v["versionId"].as_str())
                    else {
                        continue;
                    };
                    regs.entry(rid.to_string())
                        .or_default()
                        .versions
                        .insert(vid.to_string(), v.clone());
                }
            }
            _ => {}
        }
    }

    let mut out: Vec<Value> = regs
        .into_iter()
        .filter(|(_, st)| !st.deleted && !st.scope_url.is_empty())
        .filter(|(_, st)| match origin {
            Some(o) => url_origin(&st.scope_url).as_deref() == Some(o),
            None => true,
        })
        .map(|(id, st)| {
            let best = st
                .versions
                .values()
                .filter(|v| v["status"].as_str() != Some("redundant"))
                .max_by_key(|v| status_rank(v["status"].as_str().unwrap_or("")));
            json!({
                "registrationId": id,
                "scopeURL": st.scope_url,
                "scriptURL": best.map(|v| v["scriptURL"].clone()).unwrap_or(json!("")),
                "status": best.map(|v| v["status"].clone()).unwrap_or(json!("redundant")),
                "runningStatus": best
                    .map(|v| v["runningStatus"].clone())
                    .unwrap_or(json!("stopped")),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a["scopeURL"]
            .as_str()
            .unwrap_or("")
            .cmp(b["scopeURL"].as_str().unwrap_or(""))
    });
    out
}

/// The version DevTools shows first: active beats waiting beats installing.
fn status_rank(status: &str) -> u8 {
    match status {
        "activated" => 5,
        "activating" => 4,
        "installed" => 3,
        "installing" => 2,
        "new" => 1,
        _ => 0,
    }
}

/// Enable the domain and collect the replayed events. Leaves the domain enabled
/// so the caller can issue unregister/update before disabling.
async fn enable_and_collect(
    client: &CdpClient,
    session_id: &str,
) -> Result<Vec<(String, Value)>, String> {
    // If the broadcast channel overflows (busy page) replayed events may be lost;
    // re-enabling replays the full state again, so retry a couple of times.
    for attempt in 0..3 {
        if attempt > 0 {
            disable(client, session_id).await;
        }
        // Subscribe before enabling so the replay is not missed.
        let mut rx = client.subscribe();
        client
            .send_command("ServiceWorker.enable", None, Some(session_id))
            .await?;

        let start = tokio::time::Instant::now();
        let hard_deadline = start + COLLECT_MAX;
        // Quiet window is measured from the last *matching* event, so unrelated
        // traffic (network, console, other tabs) cannot keep it open.
        let mut quiet_deadline = start + COLLECT_FIRST_IDLE;
        let mut events = Vec::new();
        let mut lagged = false;
        loop {
            let deadline = quiet_deadline.min(hard_deadline);
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if ev.session_id.as_deref() == Some(session_id)
                        && ev.method.starts_with("ServiceWorker.worker")
                    {
                        events.push((ev.method, ev.params));
                        quiet_deadline = tokio::time::Instant::now() + COLLECT_IDLE;
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                    lagged = true;
                    break;
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        if !lagged {
            return Ok(events);
        }
    }
    Err("ServiceWorker events were dropped (event channel overflow); try again".to_string())
}

async fn disable(client: &CdpClient, session_id: &str) {
    let _ = client
        .send_command("ServiceWorker.disable", None, Some(session_id))
        .await;
}

/// The origin whose registrations are shown/acted on: explicit `origin`, else
/// the page's. `None` for opaque pages (about:blank, data:), which have none.
async fn target_origin(
    client: &CdpClient,
    session_id: &str,
    origin: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(o) = origin {
        return Ok(Some(o.to_string()));
    }
    let target = page_storage_target(client, session_id).await?;
    Ok(Some(target.origin).filter(|o| !o.is_empty()))
}

/// `service_workers`: `{registrations: [{registrationId, scopeURL, scriptURL,
/// status, runningStatus}]}` for the page's origin (or `origin`).
pub async fn service_workers(
    client: &CdpClient,
    session_id: &str,
    origin: Option<&str>,
) -> Result<Value, String> {
    let Some(origin) = target_origin(client, session_id, origin).await? else {
        return Ok(json!({ "registrations": [] }));
    };
    let registrations = match enable_and_collect(client, session_id).await {
        Ok(events) => {
            disable(client, session_id).await;
            fold_events(&events, Some(&origin))
        }
        Err(_) => {
            disable(client, session_id).await;
            js_registrations(client, session_id)
                .await?
                .into_iter()
                .filter(|r| {
                    r["scopeURL"].as_str().and_then(url_origin).as_deref() == Some(&origin)
                })
                .collect()
        }
    };
    Ok(json!({ "registrations": registrations }))
}

/// `service_worker_unregister` / `service_worker_update`: `{ok: true}`.
/// Only registrations of the page's origin (or `origin`) can be targeted.
pub async fn service_worker_command(
    client: &CdpClient,
    session_id: &str,
    scope_url: &str,
    origin: Option<&str>,
    unregister: bool,
) -> Result<Value, String> {
    let not_found = || format!("No service worker registration with scope {}", scope_url);
    let Some(origin) = target_origin(client, session_id, origin).await? else {
        return Err(not_found());
    };
    if url_origin(scope_url).as_deref() != Some(&origin) {
        return Err(format!(
            "Scope {} is not in the inspected origin {}",
            scope_url, origin
        ));
    }
    let events = match enable_and_collect(client, session_id).await {
        Ok(ev) => ev,
        Err(_) => {
            disable(client, session_id).await;
            return js_command(client, session_id, scope_url, unregister).await;
        }
    };
    // Chrome silently ignores unknown scopes; report it instead.
    let known = fold_events(&events, Some(&origin))
        .iter()
        .any(|r| r["scopeURL"].as_str() == Some(scope_url));
    let result = if !known {
        Err(not_found())
    } else {
        let method = if unregister {
            "ServiceWorker.unregister"
        } else {
            "ServiceWorker.updateRegistration"
        };
        client
            .send_command(method, Some(json!({ "scopeURL": scope_url })), Some(session_id))
            .await
            .map(|_| json!({ "ok": true }))
    };
    disable(client, session_id).await;
    result
}

// ---------------------------------------------------------------------------
// JS fallback
// ---------------------------------------------------------------------------

async fn eval_await(client: &CdpClient, session_id: &str, js: &str) -> Result<Value, String> {
    let res = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({ "expression": js, "awaitPromise": true, "returnByValue": true })),
            Some(session_id),
        )
        .await?;
    if let Some(ex) = res.get("exceptionDetails") {
        let msg = ex["exception"]["description"]
            .as_str()
            .or_else(|| ex["text"].as_str())
            .unwrap_or("evaluation failed");
        return Err(msg.to_string());
    }
    Ok(res["result"]["value"].clone())
}

async fn js_registrations(client: &CdpClient, session_id: &str) -> Result<Vec<Value>, String> {
    let js = r#"(async () => {
        if (!('serviceWorker' in navigator)) return [];
        const regs = await navigator.serviceWorker.getRegistrations();
        return regs.map((r, i) => {
            const w = r.active || r.waiting || r.installing;
            return {
                registrationId: '',
                scopeURL: r.scope,
                scriptURL: w ? w.scriptURL : '',
                status: w ? w.state : 'redundant',
                runningStatus: '',
            };
        });
    })()"#;
    let v = eval_await(client, session_id, js).await?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

async fn js_command(
    client: &CdpClient,
    session_id: &str,
    scope_url: &str,
    unregister: bool,
) -> Result<Value, String> {
    let js = format!(
        r#"(async () => {{
            const scope = {};
            if (!('serviceWorker' in navigator)) throw new Error('No service worker registration with scope ' + scope);
            const regs = await navigator.serviceWorker.getRegistrations();
            const r = regs.find(r => r.scope === scope);
            if (!r) throw new Error('No service worker registration with scope ' + scope);
            await r.{}();
            return true;
        }})()"#,
        serde_json::to_string(scope_url).unwrap_or_default(),
        if unregister { "unregister" } else { "update" }
    );
    eval_await(client, session_id, &js).await?;
    Ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(id: &str, scope: &str, deleted: bool) -> (String, Value) {
        (
            "ServiceWorker.workerRegistrationUpdated".into(),
            json!({ "registrations": [
                { "registrationId": id, "scopeURL": scope, "isDeleted": deleted }
            ]}),
        )
    }

    fn ver(rid: &str, vid: &str, status: &str, running: &str) -> (String, Value) {
        (
            "ServiceWorker.workerVersionUpdated".into(),
            json!({ "versions": [{
                "versionId": vid, "registrationId": rid,
                "scriptURL": format!("http://127.0.0.1:1/sw{}.js", vid),
                "status": status, "runningStatus": running
            }]}),
        )
    }

    #[test]
    fn folds_registration_with_best_version() {
        let ev = vec![
            reg("1", "http://127.0.0.1:1/", false),
            ver("1", "10", "activated", "running"),
            ver("1", "11", "installed", "stopped"),
            ver("1", "9", "redundant", "stopped"),
        ];
        let r = fold_events(&ev, None);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0],
            json!({
                "registrationId": "1",
                "scopeURL": "http://127.0.0.1:1/",
                "scriptURL": "http://127.0.0.1:1/sw10.js",
                "status": "activated",
                "runningStatus": "running",
            })
        );
    }

    #[test]
    fn later_version_update_replaces_earlier() {
        let ev = vec![
            reg("1", "http://h/", false),
            ver("1", "5", "installing", "starting"),
            ver("1", "5", "activated", "running"),
        ];
        let r = fold_events(&ev, None);
        assert_eq!(r[0]["status"], "activated");
        assert_eq!(r[0]["runningStatus"], "running");
    }

    #[test]
    fn drops_deleted_and_filters_origin() {
        let ev = vec![
            reg("1", "http://a.test/", false),
            reg("2", "http://b.test/x/", false),
            reg("3", "http://a.test/gone/", false),
            reg("3", "http://a.test/gone/", true),
        ];
        let all = fold_events(&ev, None);
        assert_eq!(all.len(), 2);
        let a = fold_events(&ev, Some("http://a.test"));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0]["scopeURL"], "http://a.test/");
        // No version known yet: still listed, marked redundant/stopped.
        assert_eq!(a[0]["status"], "redundant");
        assert_eq!(a[0]["scriptURL"], "");
    }

    #[test]
    fn ignores_unrelated_and_malformed_events() {
        let ev = vec![
            ("ServiceWorker.workerErrorReported".to_string(), json!({})),
            (
                "ServiceWorker.workerVersionUpdated".to_string(),
                json!({ "versions": [{ "versionId": "1" }] }),
            ),
            ("ServiceWorker.workerRegistrationUpdated".to_string(), json!({})),
        ];
        assert!(fold_events(&ev, None).is_empty());
    }
}
