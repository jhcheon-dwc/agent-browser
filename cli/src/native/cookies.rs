use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::cdp::client::CdpClient;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    #[serde(default)]
    pub expires: f64,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

pub async fn get_cookies(
    client: &CdpClient,
    session_id: &str,
    urls: Option<Vec<String>>,
) -> Result<Vec<Cookie>, String> {
    let params = match urls {
        Some(ref u) if !u.is_empty() => json!({ "urls": u }),
        _ => json!({}),
    };

    let result = client
        .send_command("Network.getCookies", Some(params), Some(session_id))
        .await?;

    let cookies: Vec<Cookie> = result
        .get("cookies")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    Ok(cookies)
}

pub async fn set_cookies(
    client: &CdpClient,
    session_id: &str,
    cookies: Vec<Value>,
    current_url: Option<&str>,
) -> Result<(), String> {
    let cookies: Vec<Value> = cookies
        .into_iter()
        .map(|mut c| {
            // Auto-fill url if no domain/path/url provided
            if c.get("url").is_none() && c.get("domain").is_none() && current_url.is_some() {
                c.as_object_mut().map(|m| {
                    m.insert(
                        "url".to_string(),
                        Value::String(current_url.unwrap().to_string()),
                    )
                });
            }
            c
        })
        .collect();

    client
        .send_command(
            "Network.setCookies",
            Some(json!({ "cookies": cookies })),
            Some(session_id),
        )
        .await?;

    Ok(())
}

pub async fn clear_cookies(client: &CdpClient, session_id: &str) -> Result<(), String> {
    client
        .send_command_no_params("Network.clearBrowserCookies", Some(session_id))
        .await?;
    Ok(())
}

// app-storage
/// Delete one cookie by name. CDP matches on `url` or `domain`; with neither
/// given, fall back to the page's current URL.
pub async fn delete_cookie(
    client: &CdpClient,
    session_id: &str,
    name: &str,
    domain: Option<&str>,
    path: Option<&str>,
    url: Option<&str>,
    current_url: Option<&str>,
) -> Result<(), String> {
    let params = delete_cookie_params(name, domain, path, url, current_url);
    client
        .send_command("Network.deleteCookies", Some(params), Some(session_id))
        .await?;
    Ok(())
}

fn delete_cookie_params(
    name: &str,
    domain: Option<&str>,
    path: Option<&str>,
    url: Option<&str>,
    current_url: Option<&str>,
) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("name".to_string(), json!(name));
    if let Some(d) = domain {
        params.insert("domain".to_string(), json!(d));
    }
    if let Some(p) = path {
        params.insert("path".to_string(), json!(p));
    }
    let url = url.or(if domain.is_none() { current_url } else { None });
    if let Some(u) = url {
        params.insert("url".to_string(), json!(u));
    }
    Value::Object(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_cookie_params_defaults_to_current_url() {
        let p = delete_cookie_params("sid", None, None, None, Some("http://h/x"));
        assert_eq!(p, json!({ "name": "sid", "url": "http://h/x" }));
    }

    #[test]
    fn delete_cookie_params_domain_skips_current_url() {
        let p = delete_cookie_params("sid", Some(".h"), Some("/"), None, Some("http://h/x"));
        assert_eq!(p, json!({ "name": "sid", "domain": ".h", "path": "/" }));
    }

    #[test]
    fn delete_cookie_params_explicit_url_wins() {
        let p = delete_cookie_params("sid", Some(".h"), None, Some("http://o/"), Some("http://h/"));
        assert_eq!(p, json!({ "name": "sid", "domain": ".h", "url": "http://o/" }));
    }
}
