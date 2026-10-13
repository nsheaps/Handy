//! Tauri commands behind the "Transcription API server" settings section.
//!
//! The token never appears in settings, status or events: it is returned only
//! by [`reveal_transcription_api_token`] and [`rotate_transcription_api_token`],
//! when the user asks for it.

use crate::settings::{get_settings, write_settings};
use crate::transcription_api::{config, TranscriptionApiManager, TranscriptionApiStatus};
use std::sync::Arc;
use tauri::{AppHandle, State};

/// Longest keep-warm window the UI may set (24 hours).
const MAX_KEEP_WARM_SECS: u64 = 24 * 60 * 60;

#[tauri::command]
#[specta::specta]
pub fn get_transcription_api_status(
    manager: State<'_, Arc<TranscriptionApiManager>>,
) -> TranscriptionApiStatus {
    manager.status()
}

#[tauri::command]
#[specta::specta]
pub async fn set_transcription_api_enabled(
    app: AppHandle,
    manager: State<'_, Arc<TranscriptionApiManager>>,
    enabled: bool,
) -> Result<TranscriptionApiStatus, String> {
    let mut settings = get_settings(&app);
    settings.transcription_api.enabled = enabled;
    write_settings(&app, settings);
    manager.apply().await;
    Ok(manager.status())
}

/// Change where the server listens. A non-loopback address is accepted only
/// together with `allow_non_loopback`, which the UI sets after the user
/// confirms the exposure; moving back to loopback clears that confirmation.
#[tauri::command]
#[specta::specta]
pub async fn set_transcription_api_binding(
    app: AppHandle,
    manager: State<'_, Arc<TranscriptionApiManager>>,
    bind_address: String,
    port: u16,
    allow_non_loopback: bool,
) -> Result<TranscriptionApiStatus, String> {
    let ip = config::parse_ip(&bind_address).map_err(|e| e.to_string())?;
    if port < config::MIN_UNPRIVILEGED_PORT {
        return Err(config::ConfigError::InvalidPort.to_string());
    }
    if !ip.is_loopback() && !allow_non_loopback {
        return Err(config::ConfigError::NonLoopbackNotConfirmed.to_string());
    }
    let mut settings = get_settings(&app);
    settings.transcription_api.bind_address = ip.to_string();
    settings.transcription_api.port = port;
    settings.transcription_api.allow_non_loopback = !ip.is_loopback() && allow_non_loopback;
    write_settings(&app, settings);
    manager.apply().await;
    Ok(manager.status())
}

#[tauri::command]
#[specta::specta]
pub fn set_transcription_api_keep_warm_secs(app: AppHandle, secs: u32) {
    let mut settings = get_settings(&app);
    settings.transcription_api.keep_warm_secs = (secs as u64).min(MAX_KEEP_WARM_SECS);
    write_settings(&app, settings);
}

/// Show the current token so the user can copy it into a client.
#[tauri::command]
#[specta::specta]
pub async fn reveal_transcription_api_token(
    manager: State<'_, Arc<TranscriptionApiManager>>,
) -> Result<String, String> {
    manager.reveal_token().await
}

/// Replace the token. The old one stops working at once.
#[tauri::command]
#[specta::specta]
pub async fn rotate_transcription_api_token(
    manager: State<'_, Arc<TranscriptionApiManager>>,
) -> Result<String, String> {
    manager.rotate_token().await
}
