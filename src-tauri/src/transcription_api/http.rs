//! The OpenAI-compatible HTTP surface: `POST /v1/audio/transcriptions` and
//! `GET /v1/models`.
//!
//! Every request needs `Authorization: Bearer <token>`. No CORS headers are
//! ever sent, so browsers cannot call the API cross-origin.

use super::audio::{self, AudioError};
use crate::managers::transcription::{
    ApiRequest, ApiRunControl, ApiRunError, ApiTranscript, TranscriptionManager,
};
use crate::settings::get_settings;
use axum::{
    extract::{DefaultBodyLimit, Multipart, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tokio::sync::{watch, Semaphore};

/// Largest accepted request body (the uploaded audio plus form fields).
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// One request running plus this many waiting; further requests get 429.
pub const MAX_QUEUED: usize = 4;
/// Reading the multipart body must finish within this.
const BODY_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a request may wait for its turn (behind other API requests or
/// desktop dictation) before it is answered 503.
pub const QUEUE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct ApiState {
    pub app: AppHandle,
    pub transcription: Arc<TranscriptionManager>,
    pub credential: super::auth::Credential,
    /// Requests currently held by the server: one running plus the queue.
    admission: Arc<Semaphore>,
    /// Only one request uses the engine at a time.
    run_slot: Arc<Semaphore>,
    /// Flips to true when the server is stopping.
    stopping: watch::Receiver<bool>,
}

impl ApiState {
    pub fn new(
        app: AppHandle,
        transcription: Arc<TranscriptionManager>,
        credential: super::auth::Credential,
        stopping: watch::Receiver<bool>,
    ) -> Self {
        Self {
            app,
            transcription,
            credential,
            admission: Arc::new(Semaphore::new(1 + MAX_QUEUED)),
            run_slot: Arc::new(Semaphore::new(1)),
            stopping,
        }
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/audio/transcriptions", post(transcribe))
        .route("/v1/models", get(list_models))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors

struct ApiError {
    status: StatusCode,
    kind: &'static str,
    code: &'static str,
    message: String,
    param: Option<String>,
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(
        status: StatusCode,
        kind: &'static str,
        code: &'static str,
        msg: impl Into<String>,
    ) -> Self {
        Self {
            status,
            kind,
            code,
            message: msg.into(),
            param: None,
            retry_after: None,
        }
    }

    fn bad_request(code: &'static str, param: &str, msg: impl Into<String>) -> Self {
        let mut e = Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", code, msg);
        e.param = Some(param.to_string());
        e
    }

    fn server(msg: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "transcription_failed",
            msg,
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "param": self.param,
                "code": self.code,
            }
        });
        let mut resp = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

impl From<ApiRunError> for ApiError {
    fn from(e: ApiRunError) -> Self {
        match e {
            ApiRunError::ModelNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_found",
                "The model does not exist. List the available ids with GET /v1/models.",
            ),
            ApiRunError::ModelNotDownloaded => Self::new(
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_downloaded",
                "The model is not downloaded in Handy. The API never downloads models.",
            ),
            ApiRunError::Unsupported(msg) => {
                Self::bad_request("unsupported_parameter", "model", msg)
            }
            ApiRunError::Busy => {
                let mut e = Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server_error",
                    "busy",
                    "Handy is busy with desktop use. Retry shortly.",
                );
                e.retry_after = Some(5);
                e
            }
            ApiRunError::Cancelled => {
                let mut e = Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server_error",
                    "cancelled",
                    "The request was interrupted (server stopping or cancelled in Handy). Retry.",
                );
                e.retry_after = Some(2);
                e
            }
            ApiRunError::Failed(msg) => Self::server(msg),
        }
    }
}

// ---------------------------------------------------------------------------
// Auth

async fn require_bearer(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(super::auth::parse_bearer);
    if presented.is_some_and(|t| state.credential.verify(t)) {
        return next.run(req).await;
    }
    let mut resp = ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_request_error",
        "invalid_api_key",
        "Missing or invalid bearer token.",
    )
    .into_response();
    resp.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    resp
}

// ---------------------------------------------------------------------------
// GET /v1/models

async fn list_models(State(state): State<ApiState>) -> Json<Value> {
    let tm = state.transcription.clone();
    let models = tokio::task::spawn_blocking(move || tm.api_available_models())
        .await
        .unwrap_or_default();
    let data: Vec<Value> = models
        .into_iter()
        .map(|(id, name)| json!({"id": id, "object": "model", "created": 0, "owned_by": "handy", "name": name}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

// ---------------------------------------------------------------------------
// POST /v1/audio/transcriptions

#[derive(Debug, Default)]
struct Form {
    file: Option<Vec<u8>>,
    model: Option<String>,
    language: Option<String>,
    prompt: Option<String>,
    response_format: Option<String>,
    granularities: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Json,
    VerboseJson,
}

async fn read_form(mut mp: Multipart) -> Result<Form, ApiError> {
    let mut form = Form::default();
    loop {
        let field = mp.next_field().await.map_err(|e| {
            ApiError::bad_request(
                "invalid_multipart",
                "file",
                format!("Could not read the multipart body: {e}"),
            )
        })?;
        let Some(field) = field else { break };
        let name = field.name().unwrap_or_default().to_string();
        let bad = |e: axum::extract::multipart::MultipartError| {
            ApiError::bad_request(
                "invalid_multipart",
                &name,
                format!("Could not read field '{name}': {e}"),
            )
        };
        match name.as_str() {
            "file" => form.file = Some(field.bytes().await.map_err(bad)?.to_vec()),
            "model" => form.model = Some(field.text().await.map_err(bad)?),
            "language" => form.language = Some(field.text().await.map_err(bad)?),
            "prompt" => form.prompt = Some(field.text().await.map_err(bad)?),
            "response_format" => form.response_format = Some(field.text().await.map_err(bad)?),
            "timestamp_granularities[]" | "timestamp_granularities" => {
                form.granularities.push(field.text().await.map_err(bad)?)
            }
            // The only harmless value of a parameter we do not implement.
            "stream" => {
                if field.text().await.map_err(bad)?.trim() != "false" {
                    return Err(ApiError::bad_request(
                        "unsupported_parameter",
                        "stream",
                        "Streaming responses are not supported.",
                    ));
                }
            }
            other => {
                return Err(ApiError::bad_request(
                    "unsupported_parameter",
                    other,
                    format!(
                        "The parameter '{other}' is not supported. Supported: file, model, language, prompt, response_format, timestamp_granularities[]."
                    ),
                ))
            }
        }
    }
    Ok(form)
}

fn parse_format(value: Option<&str>) -> Result<Format, ApiError> {
    match value.map(str::trim) {
        None | Some("") | Some("json") => Ok(Format::Json),
        Some("verbose_json") => Ok(Format::VerboseJson),
        Some(other) => Err(ApiError::bad_request(
            "unsupported_value",
            "response_format",
            format!("response_format '{other}' is not supported. Use json or verbose_json."),
        )),
    }
}

/// (segments wanted, words wanted, segments asked for explicitly)
fn parse_granularities(format: Format, values: &[String]) -> Result<(bool, bool, bool), ApiError> {
    if values.is_empty() {
        // OpenAI's default for verbose_json is segment-level timestamps; they
        // are provided when the model can, and omitted otherwise.
        return Ok((format == Format::VerboseJson, false, false));
    }
    if format != Format::VerboseJson {
        return Err(ApiError::bad_request(
            "unsupported_value",
            "timestamp_granularities[]",
            "timestamp_granularities[] requires response_format=verbose_json.",
        ));
    }
    let (mut segments, mut words) = (false, false);
    for v in values {
        match v.trim() {
            "segment" => segments = true,
            "word" => words = true,
            other => {
                return Err(ApiError::bad_request(
                    "unsupported_value",
                    "timestamp_granularities[]",
                    format!("Unknown granularity '{other}'. Use segment or word."),
                ))
            }
        }
    }
    Ok((segments, words, segments))
}

/// Sets the flag when dropped: the client left or the handler was cancelled.
struct AbandonOnDrop(Arc<AtomicBool>);

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn transcribe(
    State(state): State<ApiState>,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    if *state.stopping.borrow() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "shutting_down",
            "The transcription API is shutting down.",
        ));
    }
    // Admission: a bounded number of requests are held at all.
    let admission = state.admission.clone().try_acquire_owned().map_err(|_| {
        let mut e = ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "queue_full",
            "Too many transcription requests are waiting. Retry shortly.",
        );
        e.retry_after = Some(10);
        e
    })?;

    let form = tokio::time::timeout(BODY_TIMEOUT, read_form(multipart))
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "invalid_request_error",
                "body_timeout",
                "Timed out reading the request body.",
            )
        })??;

    let bytes = form.file.filter(|f| !f.is_empty()).ok_or_else(|| {
        ApiError::bad_request(
            "missing_file",
            "file",
            "The 'file' field is required and must not be empty.",
        )
    })?;
    let model_id = form
        .model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request("missing_model", "model", "The 'model' field is required.")
        })?;
    let format = parse_format(form.response_format.as_deref())?;
    let (want_segments, want_words, segments_explicit) =
        parse_granularities(format, &form.granularities)?;
    let language = form
        .language
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    let prompt = form.prompt.filter(|p| !p.trim().is_empty());

    // Wait for the single engine slot, leaving if the server stops.
    let abandon = Arc::new(AtomicBool::new(false));
    let _abandon_on_drop = AbandonOnDrop(abandon.clone());
    let mut stopping = state.stopping.clone();
    let run_slot = tokio::select! {
        slot = tokio::time::timeout(QUEUE_TIMEOUT, state.run_slot.clone().acquire_owned()) => match slot {
            Ok(Ok(slot)) => slot,
            _ => {
                let mut e = ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "server_error", "busy", "Timed out waiting for the transcription engine. Retry shortly.");
                e.retry_after = Some(5);
                return Err(e);
            }
        },
        _ = stopping.wait_for(|s| *s) => {
            return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "server_error", "shutting_down", "The transcription API is shutting down."));
        }
    };

    let keep_warm = Duration::from_secs(get_settings(&state.app).transcription_api.keep_warm_secs);
    let ctl = ApiRunControl {
        abandon: abandon.clone(),
        queue_deadline: Instant::now() + QUEUE_TIMEOUT,
        keep_warm,
    };
    let tm = state.transcription.clone();
    let model_for_job = model_id.clone();
    // Both permits move into the job, so the bound on concurrent work holds
    // even if the client disconnects while the engine is running.
    let job = tokio::task::spawn_blocking(move || {
        let _admission = admission;
        let _run_slot = run_slot;
        let pcm = audio::decode_to_pcm16k(bytes).map_err(decode_error)?;
        tm.api_transcribe(
            ApiRequest {
                model_id: model_for_job,
                pcm,
                language,
                prompt,
                want_segments,
                want_words,
                segments_explicit,
            },
            &ctl,
        )
        .map_err(ApiError::from)
    });

    // On server stop: drop queued work, but let a started run finish so the
    // client still gets its answer within the shutdown grace period.
    let mut stopping = state.stopping.clone();
    let mut job = std::pin::pin!(job);
    let transcript = loop {
        tokio::select! {
            res = &mut job => break res.map_err(|e| ApiError::server(format!("The transcription task failed: {e}")))??,
            _ = stopping.wait_for(|s| *s) => abandon.store(true, Ordering::Release),
        }
    };
    Ok(render(format, transcript))
}

fn decode_error(e: AudioError) -> ApiError {
    match e {
        AudioError::Unsupported(_) | AudioError::Empty | AudioError::TooLong => {
            ApiError::bad_request("invalid_audio", "file", e.to_string())
        }
        AudioError::Resample(_) => ApiError::server(e.to_string()),
    }
}

fn render(format: Format, t: ApiTranscript) -> Response {
    match format {
        Format::Json => Json(json!({"text": t.text})).into_response(),
        Format::VerboseJson => {
            let mut body = json!({
                "task": "transcribe",
                "duration": t.duration_secs,
                "text": t.text,
            });
            if let Some(lang) = &t.language {
                body["language"] = json!(lang);
            }
            if let Some(segments) = &t.segments {
                body["segments"] = segments
                    .iter()
                    .enumerate()
                    .map(|(i, s)| json!({"id": i, "start": s.start, "end": s.end, "text": s.text}))
                    .collect();
            }
            if let Some(words) = &t.words {
                body["words"] = words
                    .iter()
                    .map(|w| json!({"word": w.text, "start": w.start, "end": w.end}))
                    .collect();
            }
            Json(body).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_format_parsing() {
        assert_eq!(parse_format(None).ok(), Some(Format::Json));
        assert_eq!(parse_format(Some("json")).ok(), Some(Format::Json));
        assert_eq!(
            parse_format(Some("verbose_json")).ok(),
            Some(Format::VerboseJson)
        );
        assert!(parse_format(Some("srt")).is_err());
        assert!(parse_format(Some("text")).is_err());
    }

    #[test]
    fn granularity_rules() {
        // json: nothing requested.
        assert_eq!(
            parse_granularities(Format::Json, &[]).ok(),
            Some((false, false, false))
        );
        // verbose_json default: best-effort segments.
        assert_eq!(
            parse_granularities(Format::VerboseJson, &[]).ok(),
            Some((true, false, false))
        );
        // Explicit segment/word.
        let g = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_granularities(Format::VerboseJson, &g(&["word"])).ok(),
            Some((false, true, false))
        );
        assert_eq!(
            parse_granularities(Format::VerboseJson, &g(&["segment", "word"])).ok(),
            Some((true, true, true))
        );
        // Granularities need verbose_json.
        assert!(parse_granularities(Format::Json, &g(&["word"])).is_err());
        assert!(parse_granularities(Format::VerboseJson, &g(&["char"])).is_err());
    }
}
