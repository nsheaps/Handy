//! The engine-side half of the optional transcription API server.
//!
//! The HTTP layer (`crate::transcription_api`) decodes uploads and calls
//! [`TranscriptionManager::api_transcribe`]; everything that touches the
//! models happens here, so the API shares the one supervised engine with
//! dictation and the rules for who may use it are in a single place:
//!
//! * [`InferenceGate`] arbitrates between desktop use (dictation, model
//!   switches, unloads) and API requests. Desktop work always wins: it queues
//!   ahead of any API request that has not started, and an API request never
//!   starts while a recording, stream or model load is in progress. A request
//!   that is already running is never interrupted; desktop work waits for it.
//! * An API request never downloads a model, never records, pastes, writes
//!   history or runs LLM post-processing.
//! * After a request the model is kept warm for a configurable window, even
//!   when the desktop's unload policy is "immediately".
//! * If a request had to load a model other than the one selected in Handy,
//!   that is remembered (`api_displaced`) and dictation reloads its own model
//!   before its next use.

use super::*;
use crate::managers::model::canonical_language_code;
use transcribe_cpp::TimestampKind;

/// How often a waiter re-checks conditions it cannot be woken for (desktop
/// recording state, client disconnects).
const GATE_POLL: Duration = Duration::from_millis(200);

#[derive(Default)]
struct GateState {
    api_active: bool,
    desktop_active: u32,
    desktop_waiting: u32,
}

/// Mutual exclusion between desktop use of the engine and API requests, with
/// desktop priority. Desktop permits nest (a counter), API permits do not.
#[derive(Default)]
pub struct InferenceGate {
    state: Mutex<GateState>,
    cv: Condvar,
}

pub struct DesktopPermit {
    gate: Arc<InferenceGate>,
    /// True if acquiring it had to wait for an API request to finish.
    pub waited_for_api: bool,
}

pub struct ApiPermit {
    gate: Arc<InferenceGate>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum GateWait {
    /// The deadline passed while desktop work held the engine.
    Busy,
    /// The caller abandoned the request.
    Abandoned,
}

impl InferenceGate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the engine for desktop work. Waits only for an API request that is
    /// already running.
    pub fn enter_desktop(self: &Arc<Self>) -> DesktopPermit {
        let mut s = self.lock();
        s.desktop_waiting += 1;
        let mut waited = false;
        while s.api_active {
            waited = true;
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        s.desktop_waiting -= 1;
        s.desktop_active += 1;
        DesktopPermit {
            gate: Arc::clone(self),
            waited_for_api: waited,
        }
    }

    /// Take the engine for one API request, waiting until no desktop work is
    /// active or queued and `desktop_busy` (recording, streaming, loading) is
    /// false. Gives up at `deadline` or when `abandon` is set.
    pub fn enter_api(
        self: &Arc<Self>,
        deadline: Instant,
        abandon: &AtomicBool,
        desktop_busy: impl Fn() -> bool,
    ) -> Result<ApiPermit, GateWait> {
        loop {
            if abandon.load(Ordering::Acquire) {
                return Err(GateWait::Abandoned);
            }
            let busy = desktop_busy();
            let mut s = self.lock();
            if !busy && !s.api_active && s.desktop_active == 0 && s.desktop_waiting == 0 {
                s.api_active = true;
                return Ok(ApiPermit {
                    gate: Arc::clone(self),
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(GateWait::Busy);
            }
            let wait = GATE_POLL.min(deadline - now);
            let _ = self.cv.wait_timeout(s, wait);
        }
    }

    pub fn api_active(&self) -> bool {
        self.lock().api_active
    }
}

impl Drop for DesktopPermit {
    fn drop(&mut self) {
        self.gate.lock().desktop_active -= 1;
        self.gate.cv.notify_all();
    }
}

impl Drop for ApiPermit {
    fn drop(&mut self) {
        self.gate.lock().api_active = false;
        self.gate.cv.notify_all();
    }
}

/// What the HTTP layer asks for. The audio is already 16 kHz mono.
pub struct ApiRequest {
    pub model_id: String,
    pub pcm: Vec<f32>,
    /// ISO-639-1/BCP-47 code, or `None`/"auto" for detection.
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub want_segments: bool,
    pub want_words: bool,
    /// The client asked for segments by name. Otherwise (the verbose_json
    /// default) they are included only when the model can produce them.
    pub segments_explicit: bool,
}

/// Per-request controls owned by the HTTP layer.
pub struct ApiRunControl {
    /// Set when the client went away or the server is stopping. Queued work
    /// is dropped; a run that has already started finishes and is discarded.
    pub abandon: Arc<AtomicBool>,
    /// Give up waiting for the engine at this time.
    pub queue_deadline: Instant,
    /// How long to keep the model loaded after this request.
    pub keep_warm: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiTranscript {
    pub text: String,
    /// The language used or detected, when known.
    pub language: Option<String>,
    pub duration_secs: f64,
    /// Present only when the model really produced segment timestamps.
    pub segments: Option<Vec<ApiSegment>>,
    /// Present only when the model really produced word timestamps.
    pub words: Option<Vec<ApiSegment>>,
}

#[derive(Debug, PartialEq)]
pub enum ApiRunError {
    ModelNotFound,
    ModelNotDownloaded,
    /// The request asked for something this model cannot do (language,
    /// prompt, timestamp granularity).
    Unsupported(String),
    /// The engine stayed busy with desktop use until the deadline.
    Busy,
    Cancelled,
    Failed(String),
}

fn timestamp_rank(kind: TimestampKind) -> u8 {
    match kind {
        TimestampKind::None => 0,
        TimestampKind::Segment => 1,
        TimestampKind::Word => 2,
        TimestampKind::Token | TimestampKind::Auto => 3,
    }
}

fn ms_to_secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

/// Match a requested language against what the model advertises, returning the
/// model's own spelling of it. An empty list means the model is
/// language-agnostic, so any request is passed through as a hint-free run.
fn resolve_language(
    requested: &str,
    model_languages: &[String],
) -> Result<Option<String>, ApiRunError> {
    if requested.is_empty() || requested.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    if model_languages.is_empty() {
        return Ok(None);
    }
    let wanted = canonical_language_code(requested).to_ascii_lowercase();
    model_languages
        .iter()
        .find(|l| canonical_language_code(l).to_ascii_lowercase() == wanted)
        .map(|l| Some(l.clone()))
        .ok_or_else(|| {
            ApiRunError::Unsupported(format!(
                "language '{requested}' is not supported by this model"
            ))
        })
}

impl TranscriptionManager {
    /// Public ids of models an API request may use: installed, not mid-download.
    pub fn api_available_models(&self) -> Vec<(String, String)> {
        self.model_manager
            .get_available_models()
            .into_iter()
            .filter(|m| m.is_downloaded && !m.is_downloading)
            .map(|m| (m.id, m.name))
            .collect()
    }

    /// True while the model is being held by the API keep-warm window.
    pub fn api_keep_warm_active(&self) -> bool {
        Self::now_ms() < self.api_warm_until_ms.load(Ordering::Acquire)
    }

    /// Whether dictation currently has, or is about to use, the engine.
    fn desktop_busy(&self) -> bool {
        self.router.is_open()
            || self.active_stream_worker.load(Ordering::Acquire) != 0
            || *self.is_loading.lock().unwrap_or_else(|e| e.into_inner())
            || self
                .app_handle
                .try_state::<Arc<AudioRecordingManager>>()
                .is_some_and(|a| a.is_recording())
    }

    /// Run one API transcription. Blocking: call from `spawn_blocking`.
    pub fn api_transcribe(
        &self,
        mut req: ApiRequest,
        ctl: &ApiRunControl,
    ) -> Result<ApiTranscript, ApiRunError> {
        let info = self
            .model_manager
            .get_model_info(&req.model_id)
            .ok_or(ApiRunError::ModelNotFound)?;
        if !info.is_downloaded || info.is_downloading {
            return Err(ApiRunError::ModelNotDownloaded);
        }
        let is_cpp = matches!(info.engine_type, EngineType::TranscribeCpp);
        if !is_cpp
            && (req.prompt.is_some()
                || req.want_words
                || (req.want_segments && req.segments_explicit))
        {
            return Err(ApiRunError::Unsupported(
                "this model does not support 'prompt' or timestamps".to_string(),
            ));
        }
        if req.pcm.is_empty() {
            return Ok(ApiTranscript {
                text: String::new(),
                language: None,
                duration_secs: 0.0,
                segments: req.want_segments.then(Vec::new),
                words: req.want_words.then(Vec::new),
            });
        }

        let permit = match self
            .gate
            .enter_api(ctl.queue_deadline, &ctl.abandon, || self.desktop_busy())
        {
            Ok(p) => p,
            Err(GateWait::Busy) => return Err(ApiRunError::Busy),
            Err(GateWait::Abandoned) => return Err(ApiRunError::Cancelled),
        };

        let result = self.api_run_locked(&mut req, &info, ctl);

        // Whatever happened, the engine was just used: restart the idle timer
        // and hold the model for the keep-warm window.
        self.touch_activity();
        if result.is_ok() && !ctl.keep_warm.is_zero() && self.is_model_loaded() {
            self.api_warm_until_ms.store(
                Self::now_ms() + ctl.keep_warm.as_millis() as u64,
                Ordering::Release,
            );
        }
        drop(permit);
        result
    }

    /// Body of [`api_transcribe`](Self::api_transcribe), run holding the API
    /// permit, so nothing else can load, unload or use the engine meanwhile.
    fn api_run_locked(
        &self,
        req: &mut ApiRequest,
        info: &crate::managers::model::ModelInfo,
        ctl: &ApiRunControl,
    ) -> Result<ApiTranscript, ApiRunError> {
        let model_id = req.model_id.clone();
        let model_id = model_id.as_str();
        let reload_pending = self.reload_model_on_next_use.load(Ordering::Acquire);
        let loaded_ok = self.get_current_model().as_deref() == Some(model_id)
            && self.is_model_loaded()
            && !reload_pending;
        if !loaded_ok {
            if ctl.abandon.load(Ordering::Acquire) {
                return Err(ApiRunError::Cancelled);
            }
            let selected = get_settings(&self.app_handle).selected_model;
            self.reload_model_on_next_use
                .store(false, Ordering::Release);
            // Never downloads: the model was checked to be installed above.
            self.load_model_inner(model_id, None)
                .map_err(|e| ApiRunError::Failed(e.to_string()))?;
            self.api_displaced
                .store(selected != model_id, Ordering::Release);
        }
        if ctl.abandon.load(Ordering::Acquire) {
            return Err(ApiRunError::Cancelled);
        }

        let duration_secs = req.pcm.len() as f64 / 16_000.0;
        let started = Instant::now();
        let out = match self.engine.loaded() {
            Some(loaded) => self.api_run_cpp(loaded, req, info),
            None => self.api_run_onnx(req, info),
        };
        match &out {
            Ok(t) => info!(
                "API transcription with '{}' took {:.2}s for {:.2}s of audio ({} chars)",
                model_id,
                started.elapsed().as_secs_f64(),
                duration_secs,
                t.text.chars().count()
            ),
            Err(e) => warn!("API transcription with '{}' failed: {:?}", model_id, e),
        }
        out.map(|mut t| {
            t.duration_secs = duration_secs;
            t
        })
    }

    fn api_run_cpp(
        &self,
        loaded: LoadedInfo,
        req: &mut ApiRequest,
        info: &crate::managers::model::ModelInfo,
    ) -> Result<ApiTranscript, ApiRunError> {
        let caps = &loaded.capabilities;
        let requested = req.language.as_deref().unwrap_or("auto");
        let language = match resolve_language(requested, &caps.languages)? {
            Some(l) => Some(l),
            // A model that cannot detect the language must be told one.
            None if !info.supports_language_detection && !caps.languages.is_empty() => {
                let fallback = crate::managers::model::effective_language(
                    "auto",
                    &caps.languages,
                    info.supports_language_detection,
                );
                (fallback != "auto").then_some(fallback)
            }
            None => None,
        };

        let family = match req.prompt.as_deref() {
            None => None,
            Some(p) if loaded.arch == "whisper" => Some(RunExtension::Whisper(WhisperRunOptions {
                initial_prompt: Some(p.to_string()),
                ..Default::default()
            })),
            Some(_) => {
                return Err(ApiRunError::Unsupported(
                    "this model does not support 'prompt'".to_string(),
                ))
            }
        };

        let rank = timestamp_rank(caps.max_timestamp_kind);
        let (timestamps, give_segments, give_words) = if req.want_words {
            if rank < 2 {
                return Err(ApiRunError::Unsupported(
                    "this model does not support word timestamps".to_string(),
                ));
            }
            (TimestampKind::Word, req.want_segments, true)
        } else if req.want_segments {
            if rank >= 1 {
                (TimestampKind::Segment, true, false)
            } else if req.segments_explicit {
                return Err(ApiRunError::Unsupported(
                    "this model does not support segment timestamps".to_string(),
                ));
            } else {
                (TimestampKind::None, false, false)
            }
        } else {
            (TimestampKind::None, false, false)
        };

        let run_options = RunOptions {
            task: Task::Transcribe,
            language: language.clone(),
            timestamps,
            family,
            ..Default::default()
        };
        let model_id = req.model_id.clone();
        let transcript = match self
            .engine
            .transcribe(std::mem::take(&mut req.pcm), run_options)
        {
            Ok(t) => t,
            Err(EngineError::Cancelled) => return Err(ApiRunError::Cancelled),
            Err(e) => {
                self.forget_model_if_engine_dropped(&model_id, &e.to_string());
                return Err(ApiRunError::Failed(e.to_string()));
            }
        };

        let segments = give_segments.then(|| {
            transcript
                .segments
                .iter()
                .map(|s| ApiSegment {
                    start: ms_to_secs(s.t0_ms),
                    end: ms_to_secs(s.t1_ms),
                    text: s.text.trim().to_string(),
                })
                .collect()
        });
        let words = give_words.then(|| {
            transcript
                .words
                .iter()
                .map(|w| ApiSegment {
                    start: ms_to_secs(w.t0_ms),
                    end: ms_to_secs(w.t1_ms),
                    text: w.text.trim().to_string(),
                })
                .collect()
        });
        Ok(ApiTranscript {
            text: transcript.text.trim().to_string(),
            language: transcript.language.or(language),
            duration_secs: 0.0,
            segments,
            words,
        })
    }

    fn api_run_onnx(
        &self,
        req: &ApiRequest,
        info: &crate::managers::model::ModelInfo,
    ) -> Result<ApiTranscript, ApiRunError> {
        let requested = req.language.as_deref().unwrap_or("auto");
        let language = resolve_language(requested, &info.supported_languages)?;
        let model_id = req.model_id.as_str();
        let audio = req.pcm.as_slice();

        let mut engine = self
            .lock_onnx()
            .take()
            .ok_or_else(|| ApiRunError::Failed("The model is not loaded".to_string()))?;
        let lang = language.clone();
        let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<String, String> {
            let options = TranscribeOptions {
                language: lang.clone(),
                ..Default::default()
            };
            match &mut engine {
                OnnxEngine::Parakeet(e) => e
                    .transcribe_with(audio, &ParakeetParams::default())
                    .map(|r| r.text)
                    .map_err(|e| e.to_string()),
                OnnxEngine::Moonshine(e) => e
                    .transcribe(audio, &options)
                    .map(|r| r.text)
                    .map_err(|e| e.to_string()),
                OnnxEngine::MoonshineStreaming(e) => e
                    .transcribe(audio, &options)
                    .map(|r| r.text)
                    .map_err(|e| e.to_string()),
                OnnxEngine::SenseVoice(e) => {
                    let language = lang
                        .as_deref()
                        .filter(|l| matches!(*l, "zh" | "en" | "ja" | "ko" | "yue"))
                        .map(str::to_string);
                    e.transcribe_with(
                        audio,
                        &SenseVoiceParams {
                            language,
                            use_itn: Some(true),
                        },
                    )
                    .map(|r| r.text)
                    .map_err(|e| e.to_string())
                }
                OnnxEngine::GigaAM(e) => e
                    .transcribe(audio, &options)
                    .map(|r| r.text)
                    .map_err(|e| e.to_string()),
                OnnxEngine::Canary(e) => e
                    .transcribe(audio, &options)
                    .map(|r| r.text)
                    .map_err(|e| e.to_string()),
                OnnxEngine::Cohere(e) => e
                    .transcribe(audio, &options)
                    .map(|r| r.text)
                    .map_err(|e| e.to_string()),
            }
        }));
        match outcome {
            Ok(result) => {
                self.return_engine(engine, model_id);
                let text = result.map_err(ApiRunError::Failed)?;
                Ok(ApiTranscript {
                    text: text.trim().to_string(),
                    language,
                    duration_secs: 0.0,
                    segments: None,
                    words: None,
                })
            }
            Err(payload) => {
                // The engine is in an unknown state: drop it and forget the model.
                let msg = panic_payload_message(payload.as_ref());
                error!("Transcription engine panicked during an API request: {msg}");
                *self
                    .current_model_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                let _ = self.app_handle.emit(
                    "model-state-changed",
                    ModelStateEvent {
                        event_type: "unloaded".to_string(),
                        model_id: None,
                        model_name: None,
                        error: Some(format!("Engine panicked: {msg}")),
                    },
                );
                Err(ApiRunError::Failed(
                    "The transcription engine failed; it will reload on the next request"
                        .to_string(),
                ))
            }
        }
    }

    /// Called by the idle watcher: with the "unload immediately" policy a model
    /// that was only kept loaded for the API is released once the keep-warm
    /// window ends. Returns true if it unloaded.
    pub(super) fn release_api_warm_model(&self) -> bool {
        if !self.api_warm_unload_pending.load(Ordering::Acquire) || self.api_keep_warm_active() {
            return false;
        }
        if self.desktop_busy() || self.gate.api_active() {
            return false;
        }
        self.api_warm_unload_pending.store(false, Ordering::Release);
        if self.is_model_loaded() {
            info!("API keep-warm window ended, unloading model");
            self.request_unload();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    fn soon(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn api_waits_while_desktop_holds_the_gate() {
        let gate = Arc::new(InferenceGate::default());
        let abandon = AtomicBool::new(false);
        let desktop = gate.enter_desktop();
        assert!(!desktop.waited_for_api);
        assert!(matches!(
            gate.enter_api(soon(300), &abandon, || false),
            Err(GateWait::Busy)
        ));
        drop(desktop);
        assert!(gate.enter_api(soon(300), &abandon, || false).is_ok());
    }

    #[test]
    fn api_waits_while_desktop_is_recording() {
        let gate = Arc::new(InferenceGate::default());
        let abandon = AtomicBool::new(false);
        assert!(matches!(
            gate.enter_api(soon(300), &abandon, || true),
            Err(GateWait::Busy)
        ));
    }

    #[test]
    fn desktop_waits_for_running_api_and_queues_ahead_of_next_api() {
        let gate = Arc::new(InferenceGate::default());
        let abandon = AtomicBool::new(false);
        let api = gate.enter_api(soon(100), &abandon, || false).unwrap();
        assert!(gate.api_active());

        let g = Arc::clone(&gate);
        let desktop = thread::spawn(move || g.enter_desktop().waited_for_api);
        // Give the desktop thread time to start waiting.
        thread::sleep(Duration::from_millis(150));

        // A second API request must not slip in ahead of the waiting desktop.
        assert!(matches!(
            gate.enter_api(soon(200), &abandon, || false),
            Err(GateWait::Busy)
        ));
        drop(api);
        assert!(
            desktop.join().unwrap(),
            "desktop reports it waited for the API"
        );
    }

    #[test]
    fn abandoned_waiters_stop_waiting() {
        let gate = Arc::new(InferenceGate::default());
        let abandon = AtomicBool::new(true);
        assert!(matches!(
            gate.enter_api(soon(5_000), &abandon, || false),
            Err(GateWait::Abandoned)
        ));
    }

    #[test]
    fn desktop_permits_nest() {
        let gate = Arc::new(InferenceGate::default());
        let outer = gate.enter_desktop();
        let inner = gate.enter_desktop();
        drop(inner);
        drop(outer);
        assert!(gate
            .enter_api(soon(100), &AtomicBool::new(false), || false)
            .is_ok());
    }

    #[test]
    fn language_matching_uses_the_models_spelling() {
        assert_eq!(
            resolve_language("en", &langs(&["en", "es"])).unwrap(),
            Some("en".to_string())
        );
        assert_eq!(
            resolve_language("EN", &langs(&["en"])).unwrap(),
            Some("en".to_string())
        );
        assert_eq!(resolve_language("auto", &langs(&["en"])).unwrap(), None);
        assert_eq!(resolve_language("", &langs(&["en"])).unwrap(), None);
        // Language-agnostic model: no hint is passed.
        assert_eq!(resolve_language("fr", &[]).unwrap(), None);
        assert!(matches!(
            resolve_language("fr", &langs(&["en", "es"])),
            Err(ApiRunError::Unsupported(_))
        ));
    }

    #[test]
    fn timestamp_ranks_are_ordered() {
        assert!(timestamp_rank(TimestampKind::None) < timestamp_rank(TimestampKind::Segment));
        assert!(timestamp_rank(TimestampKind::Segment) < timestamp_rank(TimestampKind::Word));
        assert!(timestamp_rank(TimestampKind::Word) < timestamp_rank(TimestampKind::Token));
    }
}
