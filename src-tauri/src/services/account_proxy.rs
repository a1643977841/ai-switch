use serde_json::Value;

pub const PROXY_URL_CONFIG_KEY: &str = "proxy_url";

const SUPPORTED_PROXY_SCHEMES: [&str; 4] = ["http", "https", "socks5", "socks5h"];

/// Trim and validate one account's proxy address.
///
/// An empty value means "no per-account proxy": the request falls back to the
/// app-wide proxy resolution. Returned URLs are kept verbatim so credentials in
/// `user:password@` survive.
pub fn normalize_proxy_url(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("proxy_url must not contain whitespace".to_string());
    }
    let parsed = url::Url::parse(trimmed).map_err(|_| {
        "proxy_url must be a valid URL such as http://127.0.0.1:7890".to_string()
    })?;
    if !SUPPORTED_PROXY_SCHEMES.contains(&parsed.scheme()) {
        return Err(format!(
            "proxy_url scheme '{}' is not supported; use http, https, socks5, or socks5h",
            parsed.scheme()
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("proxy_url must include a host".to_string());
    }
    Ok(Some(trimmed.to_string()))
}

/// The proxy an account's requests must use, or `None` to fall back to the
/// app-wide proxy. Invalid stored values are treated as absent so a hand-edited
/// config cannot take the whole pool down.
pub fn account_proxy_url(config_json: &str) -> Option<String> {
    let config: Value = serde_json::from_str(config_json).ok()?;
    let raw = config.get(PROXY_URL_CONFIG_KEY)?.as_str()?;
    normalize_proxy_url(raw).ok().flatten()
}

/// Validate the proxy field of a whole account config before it is persisted.
pub fn validate_config(config: &Value) -> Result<(), String> {
    match config.get(PROXY_URL_CONFIG_KEY) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(raw)) => normalize_proxy_url(raw).map(|_| ()),
        Some(_) => Err("proxy_url must be a string".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{account_proxy_url, normalize_proxy_url, validate_config, PROXY_URL_CONFIG_KEY};
    use serde_json::json;

    #[test]
    fn blank_means_no_account_proxy() {
        assert_eq!(normalize_proxy_url("   ").unwrap(), None);
        assert_eq!(account_proxy_url("{}"), None);
        assert_eq!(account_proxy_url(r#"{"proxy_url":"  "}"#), None);
    }

    #[test]
    fn accepts_http_https_and_socks_addresses() {
        for raw in [
            "http://127.0.0.1:7890",
            "https://proxy.example.com:8443",
            "socks5://10.0.0.1:1080",
            "socks5h://user:pass@proxy.example.com:1080",
        ] {
            assert_eq!(normalize_proxy_url(raw).unwrap().as_deref(), Some(raw));
        }
        assert_eq!(
            normalize_proxy_url("  http://127.0.0.1:7890  ")
                .unwrap()
                .as_deref(),
            Some("http://127.0.0.1:7890")
        );
    }

    #[test]
    fn rejects_unsupported_or_incomplete_addresses() {
        assert!(normalize_proxy_url("ftp://127.0.0.1:21").is_err());
        assert!(normalize_proxy_url("127.0.0.1:7890").is_err());
        assert!(normalize_proxy_url("http://").is_err());
        assert!(normalize_proxy_url("http://127.0.0.1:7890/path with space").is_err());
    }

    #[test]
    fn reads_the_proxy_from_account_config() {
        assert_eq!(
            account_proxy_url(r#"{"proxy_url":"socks5://10.0.0.1:1080"}"#).as_deref(),
            Some("socks5://10.0.0.1:1080")
        );
        assert_eq!(account_proxy_url(r#"{"proxy_url":123}"#), None);
        assert_eq!(account_proxy_url(r#"{"proxy_url":"ftp://10.0.0.1"}"#), None);
    }

    #[test]
    fn config_validation_rejects_non_string_values() {
        assert!(validate_config(&json!({})).is_ok());
        assert!(validate_config(&json!({ PROXY_URL_CONFIG_KEY: "http://127.0.0.1:7890" })).is_ok());
        assert!(validate_config(&json!({ PROXY_URL_CONFIG_KEY: "" })).is_ok());
        assert!(validate_config(&json!({ PROXY_URL_CONFIG_KEY: 7890 })).is_err());
        assert!(validate_config(&json!({ PROXY_URL_CONFIG_KEY: "ftp://127.0.0.1" })).is_err());
    }
}
