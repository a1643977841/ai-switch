use crate::adapters::route_config::{inspect_codex_login_state, CodexLoginState};
use crate::app_state::AppState;
use crate::core::settings::{get_settings_core, save_settings_core};
use crate::error::{ApiError, AppError};
use crate::models::settings::{AppSettings, AppSettingsView};
use directories::BaseDirs;
use serde::Serialize;
use tauri::State;

/// Whether "keep Codex login state" can actually work right now.
///
/// The write path refuses to produce an unauthenticated provider, so without
/// this the user would only discover a missing login after a failed write. The
/// settings card asks up front and can explain what to do first.
#[derive(Debug, Clone, Serialize)]
pub struct CodexLoginStateView {
    pub usable: bool,
    /// `chatgpt` | `missing` | `unreadable` | `not_chatgpt`, so the UI can pick
    /// wording without parsing prose.
    pub state: &'static str,
    /// The `auth_mode` Codex recorded, when the file was readable.
    pub auth_mode: Option<String>,
}

#[tauri::command]
pub async fn get_codex_login_state() -> Result<CodexLoginStateView, ApiError> {
    let home = BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .ok_or_else(|| {
            ApiError::from(AppError::Filesystem {
                code: "filesystem.home_not_found",
                message: "Could not resolve the current user home directory".to_string(),
                details: None,
                recoverable: false,
            })
        })?;
    let config_path = home.join(".codex").join("config.toml");
    let state = inspect_codex_login_state(&config_path);
    Ok(match state {
        CodexLoginState::Chatgpt => CodexLoginStateView {
            usable: true,
            state: "chatgpt",
            auth_mode: Some("chatgpt".to_string()),
        },
        CodexLoginState::Missing => CodexLoginStateView {
            usable: false,
            state: "missing",
            auth_mode: None,
        },
        CodexLoginState::Unreadable(_) => CodexLoginStateView {
            usable: false,
            state: "unreadable",
            auth_mode: None,
        },
        CodexLoginState::NotChatgpt { auth_mode } => CodexLoginStateView {
            usable: false,
            state: "not_chatgpt",
            auth_mode: Some(auth_mode),
        },
    })
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<AppSettingsView, ApiError> {
    get_settings_core(&state.paths, &state.deeplink_protocols)
        .await
        .map_err(ApiError::from)
}

#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    settings: AppSettings,
) -> Result<AppSettingsView, ApiError> {
    save_settings_core(
        &state.paths,
        &state.deeplink_protocols,
        &state.close_to_tray,
        &state.route_proxy,
        settings,
    )
    .await
    .map_err(ApiError::from)
}
