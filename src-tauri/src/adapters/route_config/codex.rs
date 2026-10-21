use super::{
    existing_text, generated_invalid, invalid_existing_config, RouteConfigInput, TargetAdapter,
    TargetInspection,
};
use crate::{error::AppError, models::platform::PlatformId};
use std::path::{Path, PathBuf};
use toml_edit::{value, Document, Item, Table};

pub(super) struct CodexAdapter;

pub(crate) const CODEX_MODEL_CATALOG_FILENAME: &str = "ai-switch-model-catalog.json";

pub(crate) fn codex_model_catalog_path(home: &Path) -> PathBuf {
    home.join(".codex").join(CODEX_MODEL_CATALOG_FILENAME)
}

impl TargetAdapter for CodexAdapter {
    fn target_key(&self) -> &'static str {
        "codex"
    }

    fn client_key(&self) -> &'static str {
        "codex"
    }

    fn client_display_name(&self) -> &'static str {
        "Codex CLI"
    }

    fn native(&self) -> bool {
        true
    }

    fn restart_required(&self) -> bool {
        false
    }

    fn requires_client_models(&self) -> bool {
        false
    }

    fn platform(&self) -> PlatformId {
        PlatformId::Codex
    }

    fn resolve_path(&self, home: &Path) -> PathBuf {
        home.join(".codex").join("config.toml")
    }

    fn render(
        &self,
        path: &Path,
        existing: Option<&[u8]>,
        input: &RouteConfigInput,
    ) -> Result<Vec<u8>, AppError> {
        let mut document = match existing {
            Some(bytes) => {
                let content = existing_text(path, "TOML", bytes)?;
                if content.trim().is_empty() {
                    Document::new()
                } else {
                    content
                        .parse::<Document>()
                        .map_err(|_| invalid_existing_config(path, "TOML", "syntax is invalid"))?
                }
            }
            None => Document::new(),
        };

        apply_managed_config(&mut document, path, input)?;
        let rendered = document.to_string();
        rendered
            .parse::<Document>()
            .map_err(|_| generated_invalid(path, "TOML"))?;
        Ok(rendered.into_bytes())
    }

    fn inspect(&self, _path: &Path, existing: Option<&[u8]>) -> TargetInspection {
        let Some(bytes) = existing else {
            return TargetInspection::missing();
        };
        let Ok(content) = std::str::from_utf8(bytes) else {
            return TargetInspection::invalid();
        };
        let Ok(document) = content.parse::<Document>() else {
            return TargetInspection::invalid();
        };

        let provider_selected = document
            .get("model_provider")
            .and_then(Item::as_str)
            .is_some_and(|value| value == "ai-switch");
        let provider_defined = document
            .get("model_providers")
            .and_then(Item::as_table)
            .and_then(|providers| providers.get("ai-switch"))
            .and_then(Item::as_table)
            .is_some();

        TargetInspection::valid(provider_selected && provider_defined)
    }
}

fn apply_managed_config(
    document: &mut Document,
    path: &Path,
    input: &RouteConfigInput,
) -> Result<(), AppError> {
    document["model_provider"] = value("ai-switch");
    document["model_catalog_json"] = value(CODEX_MODEL_CATALOG_FILENAME);
    // Resolve both values before touching the document: `existing_model` borrows
    // it, and the writes below need it mutably.
    let selected_model = document
        .get("model")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .filter(|model| input.client_models.iter().any(|item| item.id == *model))
        .or_else(|| {
            input
                .client_models
                .first()
                .map(|item| item.id.as_str())
        })
        .map(str::to_string);
    let selected_levels = selected_model
        .as_deref()
        .and_then(|model| {
            input
                .client_models
                .iter()
                .find(|item| item.id == model)
        })
        .map(|item| item.reasoning_levels.clone());
    if let Some(model) = selected_model {
        // The root-level effort is what the client starts on. Writing a fixed
        // `high` here silently overrode every model whose declared ladder tops out
        // higher: the catalog says `default_reasoning_level = "max"` and the user
        // ticked `max` in the account editor, but the client opened on `high`
        // because this key outranks it. Take the selected model's own top tier
        // instead, which is the same rule the catalog uses to pick its default.
        if let Some(levels) = selected_levels {
            match levels.last() {
                Some(top) => document["model_reasoning_effort"] = value(top),
                // A model with no tiers cannot express an effort; leaving a stale
                // value behind would send one the client cannot honour.
                None => {
                    document.remove("model_reasoning_effort");
                }
            }
        }
        document["model"] = value(model);
    }
    if document.get("model_providers").is_none() {
        document["model_providers"] = Item::Table(Table::new());
    }
    let providers = document["model_providers"].as_table_mut().ok_or_else(|| {
        invalid_existing_config(
            path,
            "TOML",
            "model_providers must be a table to add the ai-switch provider",
        )
    })?;
    if !providers.contains_key("ai-switch") {
        providers.insert("ai-switch", Item::Table(Table::new()));
    }
    let provider = providers["ai-switch"].as_table_mut().ok_or_else(|| {
        invalid_existing_config(path, "TOML", "model_providers.ai-switch must be a table")
    })?;
    apply_provider_auth(provider, input);
    provider["base_url"] = value(codex_route_proxy_base_url(&input.base_url));
    provider["wire_api"] = value("responses");
    provider.remove("api_key");
    Ok(())
}

/// Writes the provider's identity and authentication fields.
///
/// Two mutually exclusive shapes, because Codex picks its credential source from
/// them:
///
/// * Default — a plain custom provider authenticated with the local proxy key in
///   `experimental_bearer_token`. Nothing in Codex gates on it, so login-only
///   features (web search, remote compaction) stay off.
/// * `codex_keep_login_state` — `requires_openai_auth` sends Codex back to the
///   ChatGPT login in `~/.codex/auth.json` while `base_url` still points at the
///   proxy, so the session survives the gateway rewrite. `name = "OpenAI"` is
///   what keeps Codex's `is_openai()` feature gates open; without it the entry
///   is a foreign provider and those features disappear even with a valid login.
///   The bearer token must then be removed — leaving it in makes Codex prefer
///   the key over the login and the whole switch does nothing.
///
/// The login path also has to carry the platform header: with no proxy key in
/// the config, the inbound `Authorization` is the ChatGPT token, which the proxy
/// cannot map to a platform and rejects. `x-ai-switch-platform` is the proxy's
/// documented fallback and is stripped before the request goes upstream.
fn apply_provider_auth(provider: &mut Table, input: &RouteConfigInput) {
    if input.codex_keep_login_state {
        provider["name"] = value("OpenAI");
        provider["requires_openai_auth"] = value(true);
        // The local proxy speaks HTTP/SSE only. `name = "OpenAI"` would
        // otherwise re-enable Codex's WebSocket transport and every turn would
        // fail on the handshake.
        provider["supports_websockets"] = value(false);
        provider.remove("experimental_bearer_token");
        provider.remove("env_key");
        set_platform_header(provider, Some("codex"));
    } else {
        provider["name"] = value("AI Switch Route Proxy");
        provider.remove("requires_openai_auth");
        provider.remove("supports_websockets");
        provider["experimental_bearer_token"] = value(&input.route_proxy_key);
        set_platform_header(provider, None);
    }
}

/// Keeps `http_headers.x-ai-switch-platform` in sync without ever touching
/// unrelated headers the user put there by hand. Passing `None` removes only the
/// key this adapter owns, and drops the table once it is empty again.
fn set_platform_header(provider: &mut Table, platform: Option<&str>) {
    const HEADER: &str = "x-ai-switch-platform";
    match platform {
        Some(platform) => {
            if !matches!(provider.get("http_headers"), Some(Item::Table(_))) {
                provider["http_headers"] = Item::Table(Table::new());
            }
            if let Some(headers) = provider["http_headers"].as_table_mut() {
                headers[HEADER] = value(platform);
            }
        }
        None => {
            let emptied = if let Some(headers) = provider["http_headers"].as_table_mut() {
                headers.remove(HEADER);
                headers.is_empty()
            } else {
                false
            };
            if emptied {
                provider.remove("http_headers");
            }
        }
    }
}

fn codex_route_proxy_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if base_last_path_segment(trimmed).is_some_and(|segment| segment.eq_ignore_ascii_case("v1")) {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn base_last_path_segment(base_url: &str) -> Option<&str> {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let path = after_scheme.split_once('/').map(|(_, path)| path)?;
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::{codex_route_proxy_base_url, CodexAdapter};
    use crate::adapters::route_config::{ClaudeEnvPlan, RouteConfigInput, TargetAdapter};
    use std::path::Path;
    use toml_edit::Document;

    fn input(keep_login_state: bool) -> RouteConfigInput {
        RouteConfigInput {
            base_url: "http://127.0.0.1:43111".to_string(),
            route_proxy_key: "sk-ai-switch-test".to_string(),
            route_proxy_key_aliases: Vec::new(),
            claude_env: ClaudeEnvPlan::default(),
            client_models: Vec::new(),
            codex_keep_login_state: keep_login_state,
        }
    }

    fn render(existing: Option<&[u8]>, keep_login_state: bool) -> Document {
        let rendered = CodexAdapter
            .render(Path::new("config.toml"), existing, &input(keep_login_state))
            .expect("render");
        String::from_utf8(rendered)
            .expect("utf8")
            .parse::<Document>()
            .expect("valid toml")
    }

    fn provider(document: &Document) -> &toml_edit::Table {
        document["model_providers"]["ai-switch"]
            .as_table()
            .expect("provider table")
    }

    #[test]
    fn route_proxy_base_url_adds_v1_once() {
        assert_eq!(
            codex_route_proxy_base_url("http://127.0.0.1:43111"),
            "http://127.0.0.1:43111/v1"
        );
        assert_eq!(
            codex_route_proxy_base_url("http://127.0.0.1:43111/v1/"),
            "http://127.0.0.1:43111/v1"
        );
    }

    #[test]
    fn default_provider_authenticates_with_the_proxy_key() {
        let document = render(None, false);
        let provider = provider(&document);

        assert_eq!(provider["name"].as_str(), Some("AI Switch Route Proxy"));
        assert_eq!(
            provider["experimental_bearer_token"].as_str(),
            Some("sk-ai-switch-test")
        );
        // No login hand-off, so nothing may claim the ChatGPT credential.
        assert!(provider.get("requires_openai_auth").is_none());
        assert!(provider.get("supports_websockets").is_none());
        assert!(
            provider.get("http_headers").is_none(),
            "the proxy key already identifies the platform; a header would be dead weight"
        );
    }

    #[test]
    fn keeping_the_login_state_replaces_the_bearer_token() {
        let document = render(None, true);
        let provider = provider(&document);

        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
        // `name = "OpenAI"` is the `is_openai()` gate for web search and remote
        // compaction; a project id here silently disables both.
        assert_eq!(provider["name"].as_str(), Some("OpenAI"));
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(
            provider["base_url"].as_str(),
            Some("http://127.0.0.1:43111/v1")
        );
        // Both would make Codex pick the key over the login, defeating the switch.
        assert!(provider.get("experimental_bearer_token").is_none());
        assert!(provider.get("env_key").is_none());
        // The proxy speaks HTTP/SSE only, so the WebSocket transport stays off.
        assert_eq!(provider["supports_websockets"].as_bool(), Some(false));
    }

    #[test]
    fn keeping_the_login_state_still_identifies_the_platform() {
        // With the proxy key gone, the inbound Authorization is the ChatGPT
        // token, which the proxy cannot map to a platform. The header is the
        // documented fallback, so it has to be written alongside the login.
        let document = render(None, true);
        let headers = provider(&document)["http_headers"]
            .as_table()
            .expect("http_headers table");

        assert_eq!(
            headers["x-ai-switch-platform"].as_str(),
            Some("codex"),
            "without it every request fails with route_proxy.key_invalid"
        );
    }

    #[test]
    fn switching_back_to_the_proxy_key_removes_the_login_shape() {
        let existing = render(None, true).to_string();
        let document = render(Some(existing.as_bytes()), false);
        let provider = provider(&document);

        assert!(provider.get("requires_openai_auth").is_none());
        assert!(provider.get("supports_websockets").is_none());
        assert_eq!(
            provider["experimental_bearer_token"].as_str(),
            Some("sk-ai-switch-test")
        );
        assert!(
            provider.get("http_headers").is_none(),
            "the platform header was ours; it must not linger once the key is back"
        );
    }

    #[test]
    fn unrelated_http_headers_survive_both_shapes() {
        let existing = br#"[model_providers.ai-switch]
name = "AI Switch Route Proxy"
base_url = "http://127.0.0.1:43111/v1"
wire_api = "responses"
experimental_bearer_token = "sk-ai-switch-test"

[model_providers.ai-switch.http_headers]
X-Region = "cn-north"
"#;

        for keep_login_state in [true, false] {
            let document = render(Some(existing), keep_login_state);
            let headers = provider(&document)["http_headers"]
                .as_table()
                .expect("http_headers table");
            assert_eq!(
                headers["X-Region"].as_str(),
                Some("cn-north"),
                "a hand-added header must never be collateral damage"
            );
            let platform = headers.get("x-ai-switch-platform");
            if keep_login_state {
                assert_eq!(platform.and_then(|item| item.as_str()), Some("codex"));
            } else {
                assert!(platform.is_none());
            }
        }
    }
}
