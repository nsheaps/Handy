# Transcription API server

Handy can optionally serve an OpenAI-compatible transcription endpoint from
inside the running app, so other programs (for example Vexa) can use the models
Handy already has installed.

- **Off by default**, including for existing installations.
- **Not a daemon.** The endpoint exists only while Handy is running. Quitting
  Handy closes it.
- Runs in-process on the same native backend and acceleration as dictation. No
  Docker, no subprocess per request.
- Never records from the microphone, touches the clipboard, pastes, plays
  sounds, writes history, calls cloud services or runs LLM post-processing.
- Never downloads a model. The requested model must already be downloaded.

Enable it in **Settings → Advanced → Transcription API**.

## Security

- Listens on `127.0.0.1:24639` by default.
- Every request needs `Authorization: Bearer <token>`, even on loopback. The
  token is generated (256 bits from the OS CSPRNG) and kept in the OS
  credential store (Keychain, Credential Manager, Secret Service). It is never
  written to settings, logs, history or the generated bindings. Reveal, copy and
  rotate it from the settings page. Rotation takes effect immediately.
- A non-loopback address needs an explicit confirmation in the UI and a
  credential. Hostnames are rejected; use an IP literal. Traffic is plain HTTP,
  so do not expose it on an untrusted network.
- No CORS headers are sent, so browsers cannot call it cross-origin.

## Request

```
POST /v1/audio/transcriptions
Authorization: Bearer <token>
Content-Type: multipart/form-data
```

| Field                       | Notes                                                                             |
| --------------------------- | --------------------------------------------------------------------------------- |
| `file`                      | Required. wav, flac, mp3 or ogg (vorbis). Up to 64 MiB and 30 minutes.            |
| `model`                     | Required. A Handy model id that is downloaded. `GET /v1/models` lists them.       |
| `language`                  | Optional ISO code, only if the model supports language selection.                 |
| `prompt`                    | Optional, only if the model supports prompts.                                     |
| `response_format`           | `json` (default) or `verbose_json`.                                               |
| `timestamp_granularities[]` | `segment` / `word`, only with `verbose_json` and only if the model produces them. |

Unsupported values (other formats, `temperature`, unsupported granularities,
unknown fields) get a 400 with an OpenAI-style error body rather than being
silently ignored.

```
curl -H "Authorization: Bearer $TOKEN" \
  -F file=@speech.wav -F model=<model-id> \
  http://127.0.0.1:24639/v1/audio/transcriptions
# {"text":"transcribed speech"}
```

For Vexa, set its transcription URL to the endpoint shown in settings and the
API key to the token.

## Concurrency and the model

- One transcription runs at a time; up to four more requests wait (`429` when
  the queue is full, `503` if the wait times out).
- Desktop dictation has priority: a waiting dictation runs before queued API
  requests, and an in-flight API run is never interrupted by a model switch or
  unload.
- If an API request needs a different model than the one selected for
  dictation, the selected model is reloaded afterwards.
- **Keep model loaded after requests** (default 300 s) keeps the model warm
  between API requests even when the desktop policy is "unload immediately".
  `0` follows the desktop policy.

## Lifecycle

- Enabling starts the listener without restarting Handy; disabling stops new
  requests, lets running ones finish (up to 30 s) and releases the port.
- Changing the address or port restarts the listener.
- If the client disconnects, queued work is dropped; a run already inside the
  engine finishes and its result is discarded.
- Listener errors surface as a **Failed** status; they never crash Handy.
