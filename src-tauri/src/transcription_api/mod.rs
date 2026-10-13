//! Optional OpenAI-compatible transcription API served from inside Handy.
//!
//! This is a feature of the running desktop app, not a daemon: Handy must stay
//! open for the endpoint to exist. It is off by default, listens on loopback
//! unless the user deliberately says otherwise, and always requires a bearer
//! token kept in the OS credential store. See `docs/transcription-api.md`.
//!
//! * [`config`] validates the bind address and port.
//! * [`auth`] generates, stores and checks the bearer token.
//! * [`audio`] decodes uploads to 16 kHz mono PCM.
//! * [`http`] is the axum router.
//! * [`TranscriptionApiManager`] owns the listener lifecycle and publishes its
//!   status to the UI.

pub mod audio;
pub mod auth;
pub mod config;
mod http;

use crate::managers::transcription::TranscriptionManager;
use crate::settings::get_settings;
use auth::{Credential, SecretStore};
use config::ListenConfig;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::AppHandle;
use tauri_specta::Event;
use tokio::sync::watch;

/// How long a stopping server lets running requests finish before it aborts.
const STOP_GRACE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionApiState {
    Disabled,
    Starting,
    Running,
    Stopping,
    Failed,
}

/// What the settings UI shows. Never contains the token.
#[derive(Clone, Debug, Serialize, Deserialize, Type, PartialEq, Eq, tauri_specta::Event)]
pub struct TranscriptionApiStatus {
    pub state: TranscriptionApiState,
    /// The URL clients should call, once the configuration is valid.
    pub endpoint_url: Option<String>,
    /// Why the server failed, or why the configuration is not usable.
    pub error: Option<String>,
    /// Whether a bearer token exists in the secure store.
    pub has_credential: bool,
    /// Whether the configured address is reachable beyond this computer.
    pub exposed: bool,
}

struct Running {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

pub struct TranscriptionApiManager {
    app: AppHandle,
    transcription: Arc<TranscriptionManager>,
    store: Arc<dyn SecretStore>,
    credential: Credential,
    status: Mutex<TranscriptionApiStatus>,
    /// Serializes start/stop/reconfigure; holds the live listener, if any.
    running: tokio::sync::Mutex<Option<Running>>,
}

impl TranscriptionApiManager {
    pub fn new(app: AppHandle, transcription: Arc<TranscriptionManager>) -> Arc<Self> {
        Self::with_store(app, transcription, Arc::new(auth::KeyringStore))
    }

    pub fn with_store(
        app: AppHandle,
        transcription: Arc<TranscriptionManager>,
        store: Arc<dyn SecretStore>,
    ) -> Arc<Self> {
        let settings = get_settings(&app).transcription_api;
        let status = TranscriptionApiStatus {
            state: TranscriptionApiState::Disabled,
            endpoint_url: None,
            error: None,
            has_credential: false,
            exposed: config::is_exposed(&settings.bind_address),
        };
        Arc::new(Self {
            app,
            transcription,
            store,
            credential: Credential::default(),
            status: Mutex::new(status),
            running: tokio::sync::Mutex::new(None),
        })
    }

    pub fn status(&self) -> TranscriptionApiStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_status(&self, update: impl FnOnce(&mut TranscriptionApiStatus)) {
        let snapshot = {
            let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
            update(&mut s);
            s.clone()
        };
        let _ = snapshot.emit(&self.app);
    }

    /// Run the secure-store call on a blocking thread: it can talk to D-Bus or
    /// the Keychain and must not stall the UI or an async worker.
    async fn blocking_store<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SecretStore) -> Result<T, auth::SecretError> + Send + 'static,
    ) -> Result<T, String> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(store.as_ref()))
            .await
            .map_err(|e| format!("credential task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Make sure a token exists (generating one on first use) and that the
    /// server's in-memory copy of its digest is current.
    async fn ensure_credential(&self) -> Result<(), String> {
        if self.credential.is_set() {
            return Ok(());
        }
        let token = self
            .blocking_store(|store| match store.get()? {
                Some(token) => Ok(token),
                None => {
                    let token = auth::generate_token()?;
                    store.set(&token)?;
                    Ok(token)
                }
            })
            .await?;
        self.credential.set_token(&token);
        Ok(())
    }

    /// Called once at startup. Never blocks the UI: the caller spawns it.
    pub async fn initialize(self: &Arc<Self>) {
        // Load the existing token (if any) so the UI can show its state, but
        // do not create one for an installation that never used the API.
        match self.blocking_store(|s| s.get()).await {
            Ok(Some(token)) => self.credential.set_token(&token),
            Ok(None) => {}
            Err(e) => warn!("Transcription API credential store unavailable: {e}"),
        }
        self.apply().await;
    }

    /// Bring the listener in line with the persisted settings: start it,
    /// stop it, or move it. Safe to call repeatedly.
    pub async fn apply(self: &Arc<Self>) {
        let mut running = self.running.lock().await;
        let settings = get_settings(&self.app).transcription_api;
        let exposed = config::is_exposed(&settings.bind_address);

        if !settings.enabled {
            self.stop_locked(&mut running).await;
            self.set_status(|s| {
                s.state = TranscriptionApiState::Disabled;
                s.error = None;
                s.exposed = exposed;
                s.endpoint_url = ListenConfig::from_settings(&settings, true)
                    .ok()
                    .map(|c| c.endpoint_url());
                s.has_credential = self.credential.is_set();
            });
            return;
        }

        if let Err(e) = self.ensure_credential().await {
            self.stop_locked(&mut running).await;
            let msg = format!("The secure credential store is unavailable: {e}");
            self.set_status(|s| {
                s.state = TranscriptionApiState::Failed;
                s.error = Some(msg);
                s.exposed = exposed;
                s.has_credential = false;
            });
            return;
        }

        let cfg = match ListenConfig::from_settings(&settings, self.credential.is_set()) {
            Ok(cfg) => cfg,
            Err(e) => {
                self.stop_locked(&mut running).await;
                self.set_status(|s| {
                    s.state = TranscriptionApiState::Failed;
                    s.error = Some(e.to_string());
                    s.exposed = exposed;
                    s.endpoint_url = None;
                    s.has_credential = self.credential.is_set();
                });
                return;
            }
        };

        if let Some(r) = running.as_ref() {
            if r.addr == cfg.addr && !r.task.is_finished() {
                self.set_status(|s| {
                    s.state = TranscriptionApiState::Running;
                    s.error = None;
                    s.exposed = exposed;
                    s.endpoint_url = Some(cfg.endpoint_url());
                    s.has_credential = true;
                });
                return;
            }
        }
        self.stop_locked(&mut running).await;
        self.start_locked(&mut running, cfg, exposed).await;
    }

    async fn start_locked(
        self: &Arc<Self>,
        running: &mut Option<Running>,
        cfg: ListenConfig,
        exposed: bool,
    ) {
        self.set_status(|s| {
            s.state = TranscriptionApiState::Starting;
            s.error = None;
            s.exposed = exposed;
            s.endpoint_url = Some(cfg.endpoint_url());
            s.has_credential = true;
        });

        let listener = match tokio::net::TcpListener::bind(cfg.addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("Transcription API could not listen on {}: {e}", cfg.addr);
                self.set_status(|s| {
                    s.state = TranscriptionApiState::Failed;
                    s.error = Some(format!("Could not listen on {}: {e}", cfg.addr));
                });
                return;
            }
        };

        let (stop_tx, stop_rx) = watch::channel(false);
        let state = http::ApiState::new(
            self.app.clone(),
            self.transcription.clone(),
            self.credential.clone(),
            stop_rx.clone(),
        );
        let router = http::router(state);
        let manager = Arc::clone(self);
        let check_rx = stop_rx.clone();
        let mut shutdown_rx = stop_rx;
        let task = tokio::spawn(async move {
            let served = axum::serve(listener, router).with_graceful_shutdown(async move {
                let _ = shutdown_rx.wait_for(|stop| *stop).await;
            });
            let result = served.await;
            // A normal stop sets the flag first; anything else is a failure.
            if !*check_rx.borrow() {
                let msg = match result {
                    Ok(()) => "The listener stopped unexpectedly".to_string(),
                    Err(e) => format!("The listener failed: {e}"),
                };
                error!("Transcription API: {msg}");
                manager.set_status(|s| {
                    s.state = TranscriptionApiState::Failed;
                    s.error = Some(msg);
                });
            }
        });

        info!(
            "Transcription API listening on {} ({})",
            cfg.addr,
            if cfg.is_loopback() {
                "loopback only"
            } else {
                "NOT loopback"
            }
        );
        *running = Some(Running {
            addr: cfg.addr,
            stop: stop_tx,
            task,
        });
        self.set_status(|s| {
            s.state = TranscriptionApiState::Running;
            s.error = None;
        });
    }

    /// Stop admitting requests, let running ones finish (up to [`STOP_GRACE`]),
    /// release the port.
    async fn stop_locked(&self, running: &mut Option<Running>) {
        let Some(mut r) = running.take() else { return };
        self.set_status(|s| s.state = TranscriptionApiState::Stopping);
        let _ = r.stop.send(true);
        match tokio::time::timeout(STOP_GRACE, &mut r.task).await {
            Ok(_) => {}
            Err(_) => {
                warn!(
                    "Transcription API: requests still running after {STOP_GRACE:?}; aborting them"
                );
                r.task.abort();
                let _ = r.task.await;
            }
        }
        info!("Transcription API stopped");
    }

    /// Best-effort, non-blocking stop for application exit. The process is
    /// about to end, which closes the socket regardless.
    pub fn shutdown(&self) {
        if let Ok(mut running) = self.running.try_lock() {
            if let Some(r) = running.take() {
                let _ = r.stop.send(true);
                r.task.abort();
            }
        }
    }

    /// Create a new token, replacing the old one. Takes effect immediately for
    /// every later request; connections authenticated with the old one are
    /// rejected on their next request. Returns the new token once.
    pub async fn rotate_token(self: &Arc<Self>) -> Result<String, String> {
        let token = self
            .blocking_store(|store| {
                let token = auth::generate_token()?;
                store.set(&token)?;
                Ok(token)
            })
            .await?;
        self.credential.set_token(&token);
        self.set_status(|s| s.has_credential = true);
        // A server that failed for lack of a credential can now start.
        self.apply().await;
        Ok(token)
    }

    /// Read the current token for the user to copy. Creates one if none exists.
    pub async fn reveal_token(self: &Arc<Self>) -> Result<String, String> {
        let token = self
            .blocking_store(|store| match store.get()? {
                Some(t) => Ok(t),
                None => {
                    let t = auth::generate_token()?;
                    store.set(&t)?;
                    Ok(t)
                }
            })
            .await?;
        self.credential.set_token(&token);
        self.set_status(|s| s.has_credential = true);
        Ok(token)
    }
}
