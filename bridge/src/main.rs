//! HTTP/SSE bridge between a web frontend (e.g. pi-web-ui) and Loop's
//! `AgentHarness`. This process does no thinking of its own: it boots a real
//! harness the same way `loop-cli` does, then translates the harness's
//! internal `AgentEvent` stream into JSON events pushed to the browser over
//! Server-Sent Events.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::broadcast;

use loop_agent::harness::{AgentHarness, AgentHarnessPhase, HostExecutionEnv};
use loop_agent::{AgentEvent, AgentTool, AgentToolResult};
use loop_app_core::runtime::{bootstrap, BootstrapOpts};
use tower_http::cors::{Any, CorsLayer};

/// Broadcast capacity: comfortably more than one turn's worth of events
/// (start/text_delta-per-chunk/.../done plus Loop's own lifecycle wrapper
/// events). A slow consumer that falls behind by more than this drops the
/// oldest events (see `RecvError::Lagged` handling below) rather than
/// blocking the harness — never blocking the one shared harness is the
/// property that matters here.
const EVENTS_CHANNEL_CAPACITY: usize = 1024;

#[derive(Clone)]
struct AppState {
    harness: Arc<AgentHarness>,
    /// Every harness event, broadcast to whichever request is currently
    /// listening. There's exactly one `subscribe()` registered on the
    /// harness for the whole process (see `main`) — previously every
    /// `/prompt` call added its own permanent listener directly on the
    /// harness, which never got removed (`AgentHarness::subscribe` has no
    /// unsubscribe) and leaked one closure per request for the life of the
    /// process. Routing through a broadcast channel instead means each
    /// request's listener is a cheap `Receiver` that's cleaned up
    /// automatically when its SSE stream ends or the client disconnects.
    events_tx: broadcast::Sender<serde_json::Value>,
    /// Where /terminal/ws spawns its shell — the harness's own cwd, the
    /// project directory. A terminal opening at the filesystem root
    /// wouldn't be useful; this stays scoped to the project on purpose,
    /// independent of files_root below.
    cwd: PathBuf,
    /// Root the /files endpoints are scoped to. Deliberately "/" — the
    /// whole filesystem, not just the project directory — per explicit
    /// confirmation (this widens what's reachable through an
    /// unauthenticated network endpoint; not a decision to make silently).
    /// `resolve_safe_path` still canonicalizes and checks containment, so
    /// this isn't a code path that trusts input blindly — it's just that
    /// "/" makes the containment check basically unrestrictive. Same level
    /// of read access the model's own bash/read tools already have
    /// unrestricted (see "no auth, unsandboxed" below) — this exposes that
    /// same existing access through a browser panel, not a new category
    /// of it.
    files_root: PathBuf,
}

#[derive(serde::Deserialize)]
struct PromptRequest {
    text: String,
}

/// Minimal `.env` loader — no dependency pulled in for this on purpose, to
/// keep the crate's dependency footprint small for anyone vendoring/forking
/// this bridge. Reads `KEY=VALUE` lines from `.env` in the current directory
/// (blank lines and `#` comments ignored) and sets each as a process env var
/// *unless it's already set* — an explicit `FOO=bar cargo run` on the
/// command line always wins over the file, matching standard dotenv
/// semantics. Missing file is fine (most deployments won't have one — e.g.
/// CI, or a real provider configured via ~/.loop/agent/ instead).
fn load_dotenv() -> anyhow::Result<()> {
    let Ok(contents) = std::fs::read_to_string(".env") else {
        return Ok(());
    };
    for (idx, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if let Some(embedded) = embedded_assignment(value) {
            anyhow::bail!(
                ".env line {}: the value of {key} contains another assignment \
                 ({embedded}=...), so several settings were merged onto one line \
                 and none of them will be read correctly. This usually happens \
                 when a block of config lines is pasted into a single-value \
                 prompt or appended without line breaks. Put each KEY=VALUE on \
                 its own line (see .env.example).",
                idx + 1
            );
        }
        // Strip one layer of matching quotes, e.g. FOO="bar baz" — plain
        // KEY=VALUE with no quoting works fine without this.
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
    Ok(())
}

/// Returns the variable name if `value` contains what looks like a second
/// `NAME_LIKE_THIS=` assignment — i.e. several settings glued onto one line.
/// Only underscore-containing ALL-CAPS names count, so ordinary values
/// (URLs with `?a=b`, base64 padding like `abc==`) never trip it.
fn embedded_assignment(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'=' {
            continue;
        }
        let start = value[..i]
            .rfind(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .map_or(0, |p| p + 1);
        let name = &value[start..i];
        if name.len() >= 4
            && name.contains('_')
            && name.starts_with(|c: char| c.is_ascii_uppercase())
        {
            return Some(name);
        }
    }
    None
}

/// Refuses to boot with an unrecognized provider instead of letting it
/// silently become "soket" (see the call site's comment for the real
/// failure mode this catches). `configured_provider` is `None` when
/// LOOP_SERVER_PROVIDER is unset, in which case the *effective* provider is
/// whatever ~/.loop/agent/settings.json says (or "soket" if that file
/// doesn't exist either — the harness's own ultimate default), so that's
/// resolved and checked the same way.
fn validate_provider_registered(configured_provider: Option<&str>) -> anyhow::Result<()> {
    const BUILTIN_PROVIDERS: &[&str] = &["soket", "faux"];

    let agent_dir = loop_app_core::config::get_agent_dir();

    let effective_provider = match configured_provider {
        Some(p) => p.to_string(),
        None => {
            let settings_path = agent_dir.join("settings.json");
            std::fs::read_to_string(&settings_path)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("defaultProvider")?.as_str().map(str::to_string))
                .unwrap_or_else(|| "soket".to_string())
        }
    };

    if BUILTIN_PROVIDERS.contains(&effective_provider.as_str()) {
        return Ok(());
    }

    let models_path = agent_dir.join("models.json");
    let is_registered = std::fs::read_to_string(&models_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("providers")?.as_array().cloned())
        .map(|providers| {
            providers
                .iter()
                .any(|p| p.get("id").and_then(|id| id.as_str()) == Some(effective_provider.as_str()))
        })
        .unwrap_or(false);

    if !is_registered {
        anyhow::bail!(
            "provider {effective_provider:?} is not registered in {models_path:?} \
             (or that file doesn't exist). Without this check, the harness would \
             silently fall back to the built-in 'soket' provider instead of \
             failing here — you'd see 'harness ready \u{2014} provider soket' below \
             and every real message would fail with a confusing auth error \
             against a provider you never configured. Fix: either add an entry \
             for {effective_provider:?} to that file (see bridge/README.md \
             'Running against the real hosted model'), or unset \
             LOOP_SERVER_PROVIDER in .env to use 'soket' directly with \
             SOKET_API_KEY / TENSORSTUDIO_API_KEY / LOOP_API_KEY instead."
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_dotenv()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Which provider/model to run against. Defaults to whatever is already
    // configured in ~/.loop/agent/settings.json (Soket by default). Override
    // with LOOP_SERVER_PROVIDER / LOOP_SERVER_MODEL to point at a keyless
    // local provider instead (e.g. a custom "ollama" entry in
    // ~/.loop/agent/models.json). Always a real provider — no scripted/faux
    // fallback exists in this bridge.
    let provider = std::env::var("LOOP_SERVER_PROVIDER").ok();
    let model = std::env::var("LOOP_SERVER_MODEL").ok();
    let cwd = std::env::current_dir()?;

    // Resume the same Loop session across restarts instead of silently
    // starting a fresh (empty-history) one every time the process launches.
    // First run: no file yet, bootstrap creates a new session, we persist
    // its id. Later runs: read the id back and ask bootstrap to resume it.
    // If the referenced session no longer exists in Loop's store (deleted,
    // moved LOOP_CODING_AGENT_DIR, ...), bootstrap fails with a clear error
    // rather than silently discarding history — delete the session file to
    // start over deliberately.
    let session_file = std::env::var("LOOP_SERVER_SESSION_FILE")
        .unwrap_or_else(|_| ".loop-server-session-id".to_string());
    let resume_session_id = std::fs::read_to_string(&session_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(id) = &resume_session_id {
        tracing::info!("resuming session {id} (from {session_file})");
    } else {
        tracing::info!("no session file at {session_file} yet — starting a new session");
    }

    tracing::info!("booting AgentHarness (provider={provider:?}, model={model:?}, cwd={cwd:?})");

    // A real, previously-hit failure mode: LOOP_SERVER_PROVIDER pointing at a
    // custom provider (e.g. "tensorstudio-litellm") that was never actually
    // registered in ~/.loop/agent/models.json — usually because that file
    // (or all of ~/.loop) doesn't exist yet on this machine. Upstream's own
    // bootstrap() doesn't error on this; an unrecognized provider id just
    // silently falls back to the built-in "soket" default. The only visible
    // symptom is "harness ready — provider soket" a few lines down, easy to
    // miss, followed by every real message failing with a confusing auth
    // error against a provider you never configured. Checking this
    // ourselves, loudly, before bootstrap runs, turns a silent 10-minute
    // debugging session into one clear error line.
    validate_provider_registered(provider.as_deref())?;

    // Loop's harness itself (loop-agent, loop-ai) and loop-app-core's shared
    // bootstrap/runtime are used completely unmodified here — deliberately,
    // so this bridge stays a separate, swappable consumer of Loop rather
    // than a fork of it. The two steps below exist entirely in this crate
    // for that reason, working around real headless-server landmines in
    // bootstrap() purely through its existing public API (TrustStore,
    // BootstrapOpts), no source changes:
    //
    // 1. bootstrap(interactive: false) — the only other option — bails with
    //    a hard error unless a Soket-shaped key (SOKET_API_KEY /
    //    TENSORSTUDIO_API_KEY / LOOP_API_KEY) is present, even when a
    //    completely different provider (e.g. Ollama) is configured instead.
    //    interactive: true avoids that bail. Verified against the source
    //    (loop-app-core's src/runtime.rs, ensure_soket_api_key, in the
    //    upstream soketlabs/loop repo this crate depends on): the
    //    "interactive" branch just returns Ok(true) and defers actual
    //    prompting to TUI code this process never calls — so it can't hang
    //    on that path by itself.
    // 2. But interactive: true also flows into resolve_trust()'s project-
    //    trust check, which — verified live — *does* do a real blocking
    //    `rl.readline()` on stdin the first time a directory has no cached
    //    trust decision. Fine for a real terminal, fatal for a headless
    //    server with no one to answer it. Pre-seeding a trust decision
    //    before bootstrap runs (via TrustStore's own public load/get/set)
    //    makes resolve_trust's cache check short-circuit before ever
    //    reaching that prompt.
    {
        use loop_app_core::config::{get_agent_dir, trust_path, TrustStore};
        let agent_dir = get_agent_dir();
        let mut trust = TrustStore::load(trust_path(&agent_dir))?;
        if trust.get(&cwd).is_none() {
            trust.set(&cwd, true)?;
            tracing::info!("no trust decision cached for {cwd:?} — trusting it (headless server, no one to ask)");
        }
    }

    let runtime = bootstrap(BootstrapOpts {
        cwd: cwd.clone(),
        provider,
        model,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: true,
        session_id: resume_session_id,
    })
    .await?;

    std::fs::write(&session_file, &runtime.session_id)?;

    // Register our two extra tools alongside the standard 4 (read/write/edit/
    // bash). set_tools() *replaces* the whole list, so the base 4 have to be
    // rebuilt here rather than fetched from the harness — there's no public
    // getter for its current tool list, but build_tools() (the same function
    // bootstrap() itself calls) is public, so this reconstructs the identical
    // set rather than duplicating its logic.
    {
        let host_env: Arc<dyn loop_agent::harness::ExecutionEnv> = Arc::new(HostExecutionEnv::new(cwd.clone()));
        let mut tools = loop_app_core::runtime::build_tools(host_env);
        let tool_count_before = tools.len();
        tools.push(build_read_document_tool());
        tools.push(build_rag_query_tool());
        let tool_count = tools.len();
        runtime
            .harness
            .set_tools(tools)
            .await
            .map_err(|e| anyhow::anyhow!("failed to register tools: {e}"))?;
        tracing::info!(
            "registered {tool_count} tools ({tool_count_before} standard + read_document + rag_query)"
        );
    }

    tracing::info!(
        "harness ready — session {} — provider {}, model {}",
        runtime.session_id,
        runtime.settings.default_provider,
        runtime.settings.default_model
    );

    // One subscription for the life of the process (see AppState::events_tx
    // doc comment for why this replaced a per-request subscribe() call).
    let (events_tx, _) = broadcast::channel::<serde_json::Value>(EVENTS_CHANNEL_CAPACITY);
    let events_tx_sub = events_tx.clone();
    runtime.harness.subscribe(move |ev: AgentEvent| {
        let events_tx = events_tx_sub.clone();
        async move {
            // Err here just means no request is currently listening — not a
            // problem; the harness itself is never blocked by this send.
            let _ = events_tx.send(agent_event_to_json(&ev));
        }
    });

    let state = AppState {
        harness: runtime.harness,
        events_tx,
        cwd,
        files_root: PathBuf::from("/"),
    };

    // CORS origin defaults to the Vite dev server this bridge was built
    // against. Override with LOOP_SERVER_CORS_ORIGIN for a different
    // frontend origin, or set it to "*" to allow any origin (fine for a
    // throwaway local demo; never do this once this bridge is reachable
    // from anywhere but your own machine — it holds no auth of its own).
    let cors_origin =
        std::env::var("LOOP_SERVER_CORS_ORIGIN").unwrap_or_else(|_| "http://localhost:5173".to_string());
    let cors = if cors_origin == "*" {
        tracing::warn!("LOOP_SERVER_CORS_ORIGIN=* — allowing any origin; fine for a local demo only");
        CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any)
    } else {
        let origin: HeaderValue = cors_origin
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid LOOP_SERVER_CORS_ORIGIN {cors_origin:?}: {e}"))?;
        tracing::info!("CORS restricted to origin {cors_origin}");
        CorsLayer::new().allow_origin(origin).allow_methods(Any).allow_headers(Any)
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/prompt", post(prompt_handler))
        .route("/files", get(list_files))
        .route("/files/content", get(read_file_content))
        .route("/rag/query", post(rag_query_handler))
        .route("/rag/ingest", post(rag_ingest_handler))
        .route("/rag/documents", get(rag_documents_handler))
        .route("/rag/documents/:id", get(rag_document_handler))
        .route("/terminal/ws", get(terminal_ws))
        .with_state(state)
        .layer(cors);

    let port: u16 = std::env::var("LOOP_SERVER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!("loop-server listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "session_id": state.harness.session_id().await,
    }))
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_error(status: StatusCode, error: &str) -> ApiError {
    (status, Json(serde_json::json!({ "error": error })))
}

/// Resolves `relative` against `root` and rejects anything that
/// canonicalizes outside of it — the only thing standing between /files and
/// letting a browser tab read arbitrary files on this machine. Requires the
/// target to actually exist: `canonicalize()` fails on a path that doesn't,
/// which is also the correct rejection (no distinction leaked between
/// "doesn't exist" and "exists but forbidden").
fn resolve_safe_path(root: &Path, relative: &str) -> Result<PathBuf, ApiError> {
    let joined = root.join(relative.trim_start_matches('/'));
    let canonical = joined
        .canonicalize()
        .map_err(|_| api_error(StatusCode::NOT_FOUND, "not_found"))?;
    let root_canonical = root
        .canonicalize()
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "server_misconfigured"))?;
    if !canonical.starts_with(&root_canonical) {
        return Err(api_error(StatusCode::FORBIDDEN, "outside_project_root"));
    }
    Ok(canonical)
}

#[derive(serde::Deserialize)]
struct FilesQuery {
    #[serde(default)]
    path: String,
}

#[derive(serde::Serialize)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
}

/// GET /files?path=relative/dir -> directory listing, scoped to the
/// harness's cwd (the project directory). `.git`/`node_modules`/`target`
/// are filtered out — noisy, not useful to browse from a chat UI panel;
/// they're still fully reachable by the model's own `bash`/`read` tools,
/// this is purely about what this specific browser panel shows.
async fn list_files(
    State(state): State<AppState>,
    Query(q): Query<FilesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let dir = resolve_safe_path(&state.files_root, &q.path)?;
    let meta = std::fs::metadata(&dir).map_err(|_| api_error(StatusCode::NOT_FOUND, "not_found"))?;
    if !meta.is_dir() {
        return Err(api_error(StatusCode::BAD_REQUEST, "not_a_directory"));
    }

    let read_dir =
        std::fs::read_dir(&dir).map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let mut entries: Vec<FileEntry> = Vec::new();
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == "node_modules" || name == "target" {
            continue;
        }
        let Ok(entry_meta) = entry.metadata() else { continue };
        entries.push(FileEntry {
            name,
            is_dir: entry_meta.is_dir(),
            size: entry_meta.len(),
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));

    Ok(Json(serde_json::json!({ "path": q.path, "entries": entries })))
}

#[derive(serde::Deserialize)]
struct FileContentQuery {
    path: String,
}

/// Preview cap: large enough for real source files, small enough that a
/// misclick on a big log/binary doesn't try to ship megabytes to the panel.
const MAX_FILE_PREVIEW_BYTES: u64 = 256 * 1024;

/// GET /files/content?path=relative/file -> that file's content, scoped the
/// same way list_files is. Binary and over-size files return a message
/// instead of their bytes rather than erroring — the caller can still tell
/// what happened.
async fn read_file_content(
    State(state): State<AppState>,
    Query(q): Query<FileContentQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let file_path = resolve_safe_path(&state.files_root, &q.path)?;
    let meta = std::fs::metadata(&file_path).map_err(|_| api_error(StatusCode::NOT_FOUND, "not_found"))?;
    if meta.is_dir() {
        return Err(api_error(StatusCode::BAD_REQUEST, "is_a_directory"));
    }
    if meta.len() > MAX_FILE_PREVIEW_BYTES {
        return Ok(Json(serde_json::json!({
            "path": q.path,
            "size": meta.len(),
            "content": null,
            "message": format!(
                "{} bytes — larger than the {}KB preview limit.",
                meta.len(),
                MAX_FILE_PREVIEW_BYTES / 1024
            ),
        })));
    }

    let bytes =
        std::fs::read(&file_path).map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    match String::from_utf8(bytes) {
        Ok(content) => Ok(Json(serde_json::json!({ "path": q.path, "size": meta.len(), "content": content }))),
        Err(_) => Ok(Json(serde_json::json!({
            "path": q.path,
            "size": meta.len(),
            "content": null,
            "message": "Binary file — not shown.",
        }))),
    }
}

#[derive(serde::Deserialize)]
struct RagQueryRequest {
    query: String,
}

/// POST /rag/query { "query": "..." } -> direct, non-streamed RAG retrieval,
/// bypassing the model entirely. Distinct from the `rag_query` *tool*
/// (`build_rag_query_tool`, below) — that one the model decides to call
/// mid-conversation; this endpoint is a manual "just ask the RAG service"
/// path for the frontend's `/rag-query` slash command (see web/main.ts) or
/// direct curl testing, with no LLM turn involved at all. Shares the same
/// RAG_SERVICE_URL / RAG_SERVICE_API_KEY env vars and forwarding logic —
/// factoring that into one shared function was considered, but the tool's
/// closure has a different error type (`String`, for `AgentToolResult`)
/// than this handler needs (`ApiError`), so it stays duplicated rather than
/// forcing an awkward shared signature for ~15 lines of `reqwest` calls.
async fn rag_query_handler(Json(req): Json<RagQueryRequest>) -> Result<Json<serde_json::Value>, ApiError> {
    let Ok(rag_url) = std::env::var("RAG_SERVICE_URL") else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "RAG_SERVICE_URL is not configured on this bridge",
        ));
    };

    let client = reqwest::Client::new();
    let mut request = client.post(format!("{rag_url}/query")).json(&serde_json::json!({ "query": req.query }));
    if let Ok(api_key) = std::env::var("RAG_SERVICE_API_KEY") {
        request = request.bearer_auth(api_key);
    }
    let res = request
        .send()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, &format!("RAG service request failed: {e}")))?;

    let status = res.status();
    let body: serde_json::Value = res
        .json()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, &format!("RAG service returned a non-JSON response: {e}")))?;
    if !status.is_success() {
        return Err((StatusCode::BAD_GATEWAY, Json(body)));
    }
    Ok(Json(body))
}

#[derive(serde::Deserialize)]
struct RagIngestRequest {
    path: String,
    #[serde(default)]
    id: Option<String>,
}

/// POST /rag/ingest { "path": "...", "id": "optional" } -> reads a file
/// already on disk (same extraction as the `read_document` tool — PDF via
/// pdf-extract, else plain text) and forwards its full text to the RAG
/// service's own /ingest endpoint, for the frontend's `/rag-add` slash
/// command. `id` defaults to the file's name if omitted. No truncation
/// here (unlike `read_document`'s 100k-char cap for context-window safety)
/// — the RAG service does its own chunking on the full text.
async fn rag_ingest_handler(Json(req): Json<RagIngestRequest>) -> Result<Json<serde_json::Value>, ApiError> {
    let Ok(rag_url) = std::env::var("RAG_SERVICE_URL") else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "RAG_SERVICE_URL is not configured on this bridge",
        ));
    };

    let path = PathBuf::from(&req.path);
    let text = extract_document_text(&path)
        .await
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, &e))?;
    let doc_id = req.id.unwrap_or_else(|| {
        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| req.path.clone())
    });

    let client = reqwest::Client::new();
    let mut request =
        client.post(format!("{rag_url}/ingest")).json(&serde_json::json!({ "text": text, "id": doc_id }));
    if let Ok(api_key) = std::env::var("RAG_SERVICE_API_KEY") {
        request = request.bearer_auth(api_key);
    }
    let res = request
        .send()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, &format!("RAG service request failed: {e}")))?;

    let status = res.status();
    let body: serde_json::Value = res
        .json()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, &format!("RAG service returned a non-JSON response: {e}")))?;
    if !status.is_success() {
        return Err((StatusCode::BAD_GATEWAY, Json(body)));
    }
    Ok(Json(body))
}

/// GET /rag/documents -> list every document the RAG service has ingested.
/// GET /rag/documents/:id -> the exact original text of one document.
///
/// Unlike /rag/query and /rag/ingest, **this pair isn't part of the fixed
/// RAG interface contract** (see README.md "Swapping in
/// a RAG service") — most vector databases have no "list everything" or
/// "exact fetch by ID" API of their own (pure similarity search only), so
/// a RAG service needs its own separate mechanism to support these two at
/// all. The RAG service pointed at via RAG_SERVICE_URL may not implement
/// these at all — that's fine, whatever error it returns (404, or a
/// connection failure) is relayed as-is rather than these two pretending
/// to be universal when they aren't.
async fn rag_documents_handler() -> Result<Json<serde_json::Value>, ApiError> {
    let Ok(rag_url) = std::env::var("RAG_SERVICE_URL") else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "RAG_SERVICE_URL is not configured on this bridge",
        ));
    };
    rag_service_get(&rag_url, "/documents").await
}

async fn rag_document_handler(axum::extract::Path(doc_id): axum::extract::Path<String>) -> Result<Json<serde_json::Value>, ApiError> {
    let Ok(rag_url) = std::env::var("RAG_SERVICE_URL") else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "RAG_SERVICE_URL is not configured on this bridge",
        ));
    };
    rag_service_get(&rag_url, &format!("/documents/{}", urlencoding_encode(&doc_id))).await
}

/// Shared GET-with-auth logic for the two handlers above.
async fn rag_service_get(rag_url: &str, path: &str) -> Result<Json<serde_json::Value>, ApiError> {
    let client = reqwest::Client::new();
    let mut request = client.get(format!("{rag_url}{path}"));
    if let Ok(api_key) = std::env::var("RAG_SERVICE_API_KEY") {
        request = request.bearer_auth(api_key);
    }
    let res = request
        .send()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, &format!("RAG service request failed: {e}")))?;

    let status = res.status();
    let body: serde_json::Value = res
        .json()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, &format!("RAG service returned a non-JSON response: {e}")))?;
    if !status.is_success() {
        return Err((
            if status == StatusCode::NOT_FOUND { StatusCode::NOT_FOUND } else { StatusCode::BAD_GATEWAY },
            Json(body),
        ));
    }
    Ok(Json(body))
}

/// Minimal path-segment percent-encoding — just enough for a doc_id to
/// safely sit inside a URL path segment (handles the characters a filename
/// realistically contains: spaces, slashes if someone passes a nested path
/// as an id, etc). Not a full RFC 3986 implementation; this project has no
/// other need for one, so it isn't worth pulling in a crate for.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// POST /prompt { "text": "..." } -> SSE stream of JSON events.
///
/// Rejects with 409 if the harness is already mid-turn, rather than the
/// previous behavior of silently kicking off a call that would fail deep
/// inside the harness (`AgentHarnessError::Busy`) with no clear signal to
/// the caller. `AgentHarness::prompt` calls aren't meant to run concurrently
/// against one harness — this is a fast, explicit check for that instead of
/// finding out via a confusing error mid-stream.
async fn prompt_handler(
    State(state): State<AppState>,
    Json(body): Json<PromptRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<serde_json::Value>)> {
    if state.harness.phase() != AgentHarnessPhase::Idle {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "harness_busy",
                "message": "Loop is still working on a previous message. Wait for it to finish before sending another.",
            })),
        ));
    }

    // Subscribed *before* spawning the prompt call below: a broadcast
    // Receiver starts buffering from the moment it's created, so this
    // ordering guarantees we can't miss the turn's earliest events to a
    // scheduling race, regardless of when the SSE stream is first polled.
    let mut rx = state.events_tx.subscribe();

    let harness = Arc::clone(&state.harness);
    let events_tx = state.events_tx.clone();
    tokio::spawn(async move {
        match harness.prompt(body.text).await {
            Ok(_message) => {
                // Loop's own AgentEvent::AgentEnd should already have gone
                // out via subscribe(); this is a belt-and-suspenders signal
                // for the frontend to close its EventSource.
                let _ = events_tx.send(serde_json::json!({ "type": "stream_end" }));
            }
            Err(e) => {
                let _ = events_tx.send(serde_json::json!({ "type": "error", "message": e.to_string() }));
                let _ = events_tx.send(serde_json::json!({ "type": "stream_end" }));
            }
        }
    });

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(value) => {
                    // events_tx is one broadcast channel shared by every
                    // request, so without this check the stream would sit
                    // open forever after its own turn ends, waiting on
                    // events from whatever turn some *other* request starts
                    // next. Close it as soon as this turn's own end fires.
                    let is_end = value.get("type").and_then(|t| t.as_str()) == Some("stream_end");
                    yield Ok(Event::default().data(value.to_string()));
                    if is_end {
                        break;
                    }
                }
                // We fell more than EVENTS_CHANNEL_CAPACITY events behind the
                // harness (would need a very slow consumer + a very chatty
                // turn) — skip the gap and keep going rather than stalling.
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!("SSE consumer lagged, skipped {skipped} events");
                    continue;
                }
                // events_tx dropped — process is shutting down.
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Translate Loop's internal `AgentEvent` into the JSON shape the frontend
/// adapter expects. `AgentEvent` itself doesn't derive `Serialize`, so this
/// is the one place that knowledge of its shape lives.
fn agent_event_to_json(ev: &AgentEvent) -> serde_json::Value {
    use AgentEvent::*;
    match ev {
        AgentStart => serde_json::json!({ "type": "agent_start" }),
        AgentEnd { messages } => serde_json::json!({
            "type": "agent_end",
            "messages": messages,
        }),
        TurnStart => serde_json::json!({ "type": "turn_start" }),
        TurnEnd {
            message,
            tool_results,
        } => serde_json::json!({
            "type": "turn_end",
            "message": message,
            "tool_results": tool_results,
        }),
        MessageStart { message } => serde_json::json!({
            "type": "message_start",
            "message": message,
        }),
        MessageUpdate {
            assistant_message_event,
            ..
        } => serde_json::to_value(assistant_message_event).unwrap_or_else(|e| {
            serde_json::json!({ "type": "error", "message": format!("serialize failure: {e}") })
        }),
        MessageEnd { message } => serde_json::json!({
            "type": "message_end",
            "message": message,
        }),
        ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => serde_json::json!({
            "type": "tool_execution_start",
            "id": tool_call_id,
            "name": tool_name,
            "args": args,
        }),
        ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => serde_json::json!({
            "type": "tool_execution_update",
            "id": tool_call_id,
            "name": tool_name,
            "args": args,
            "partial_result": partial_result,
        }),
        ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => serde_json::json!({
            "type": "tool_execution_end",
            "id": tool_call_id,
            "name": tool_name,
            "result": result,
            "is_error": is_error,
        }),
    }
}

#[derive(serde::Deserialize)]
struct ResizeMsg {
    cols: u16,
    rows: u16,
}

/// GET /terminal/ws -> upgrades to a WebSocket carrying a real, interactive
/// shell in a real PTY (via portable-pty), spawned in the harness's cwd.
/// This is the same level of access the model's own `bash` tool already
/// has — a human typing directly instead of the model deciding what to
/// run — not a new category of risk for this bridge (see the README's
/// "no auth, unsandboxed" note).
///
/// Protocol: client sends Binary frames for raw keystrokes (written
/// straight to the PTY) and Text frames as JSON `{"cols","rows"}` for
/// resize; server sends Binary frames of raw PTY output. Reading/writing
/// the PTY is blocking (portable-pty's API, not tokio's), so both run on
/// dedicated OS threads bridged to the async socket via channels rather
/// than blocking the runtime.
async fn terminal_ws(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_terminal_socket(socket, state.cwd.clone()))
}

async fn handle_terminal_socket(mut socket: WebSocket, cwd: PathBuf) {
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("\r\nfailed to open pty: {e}\r\n"))).await;
            return;
        }
    };

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.cwd(&cwd);

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("\r\nfailed to spawn {shell}: {e}\r\n"))).await;
            return;
        }
    };
    // Dropping the slave end in the parent process is required — otherwise
    // the PTY never sees EOF-equivalent conditions correctly and the
    // reader thread below can hang around after the child actually exits.
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("\r\nfailed to open pty reader: {e}\r\n"))).await;
            return;
        }
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("\r\nfailed to open pty writer: {e}\r\n"))).await;
            return;
        }
    };
    let master = pair.master;

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(data) = in_rx.recv() {
            if writer.write_all(&data).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    loop {
        tokio::select! {
            chunk = out_rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        if socket.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if in_tx.send(data).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(resize) = serde_json::from_str::<ResizeMsg>(&text) {
                            let _ = master.resize(PtySize {
                                rows: resize.rows,
                                cols: resize.cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    let _ = child.kill();
}

/// Reads and extracts text from a document on disk — PDF via a real text
/// extractor, or plain UTF-8 text files directly. DOCX/XLSX/PPTX aren't
/// supported yet; returns a clear error for those rather than garbage.
/// Path resolution is deliberately unrestricted (same as bash/read, which
/// the model already has) — not a new access boundary. Shared by both the
/// `read_document` tool below and the `/rag/ingest` HTTP endpoint (see
/// `rag_ingest_handler`) — factored out so PDF-vs-text handling exists in
/// exactly one place rather than being copy-pasted between them.
async fn extract_document_text(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }

    let is_pdf = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false);

    if is_pdf {
        // pdf-extract is synchronous/blocking — real parsing work, not just
        // I/O — so it runs on a blocking thread rather than tying up the
        // async runtime.
        let path_for_blocking = path.to_path_buf();
        tokio::task::spawn_blocking(move || pdf_extract::extract_text(&path_for_blocking))
            .await
            .map_err(|e| format!("pdf extraction task panicked: {e}"))?
            .map_err(|e| format!("failed to extract PDF text from {}: {e}", path.display()))
    } else {
        std::fs::read_to_string(path).map_err(|e| {
            format!(
                "failed to read {} as text (not a .pdf, and not valid UTF-8 text — \
                 DOCX/XLSX/PPTX aren't supported yet): {e}",
                path.display()
            )
        })
    }
}

fn build_read_document_tool() -> AgentTool {
    const MAX_DOC_CHARS: usize = 100_000;

    AgentTool::simple(
        "read_document",
        "Read Document",
        "Read and extract text from a document file on disk so it can be summarized \
         or used to answer questions. Supports PDF (real text extraction, not raw \
         bytes) and plain text files (.txt, .md, code, etc). Not yet supported: \
         DOCX, XLSX, PPTX — returns a clear error for those.",
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path, or path relative to the current working directory, to the document to read."
                }
            }
        }),
        |_id, args, _cancel, _on_update| async move {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing required 'path' argument".to_string())?;
            let path = std::path::PathBuf::from(path);
            let text = extract_document_text(&path).await?;

            let char_count = text.chars().count();
            let text = if char_count > MAX_DOC_CHARS {
                let mut truncated: String = text.chars().take(MAX_DOC_CHARS).collect();
                truncated.push_str(&format!(
                    "\n\n[...truncated: document is {char_count} characters, \
                     exceeds the {MAX_DOC_CHARS}-character preview limit...]"
                ));
                truncated
            } else {
                text
            };

            Ok(AgentToolResult::text(text))
        },
    )
}

/// Queries a configurable RAG (retrieval-augmented generation) service —
/// a real, callable tool the model can choose to invoke, not automatic
/// context injection. Configured via RAG_SERVICE_URL; when unset this
/// honestly reports that rather than fabricating retrieved content.
///
/// No RAG service is bundled with this project — this is deliberately
/// provider-agnostic. Point RAG_SERVICE_URL at any service that implements
/// the interface below and it works with zero code changes here; if its
/// real API differs, put a thin translating adapter in front of it rather
/// than editing this tool. The request/response shape (`POST {url}/query`,
/// `{"query": "..."}` → any JSON, `{"answer", "matches"}` renders nicest)
/// is a convention for this project, not an industry standard.
/// RAG_SERVICE_API_KEY is sent as `Authorization: Bearer <key>` if set —
/// optional, since not every RAG service needs auth.
fn build_rag_query_tool() -> AgentTool {
    AgentTool::simple(
        "rag_query",
        "RAG Query",
        "Query the configured RAG (retrieval-augmented generation) service for \
         context relevant to a question — e.g. from an internal knowledge base or \
         document store. Only performs a real retrieval when a RAG service has been \
         configured server-side; otherwise says so plainly instead of making \
         anything up.",
        serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The question or search query to retrieve relevant context for."
                }
            }
        }),
        |_id, args, _cancel, _on_update| async move {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing required 'query' argument".to_string())?
                .to_string();

            let Ok(rag_url) = std::env::var("RAG_SERVICE_URL") else {
                return Ok(AgentToolResult::text(
                    "RAG service is not configured on this bridge (RAG_SERVICE_URL is \
                     unset) — no real retrieval was performed, this is not fabricated \
                     context. Set RAG_SERVICE_URL to a real endpoint to enable this \
                     tool for real."
                        .to_string(),
                ));
            };

            let client = reqwest::Client::new();
            let mut req = client
                .post(format!("{rag_url}/query"))
                .json(&serde_json::json!({ "query": query }));
            if let Ok(api_key) = std::env::var("RAG_SERVICE_API_KEY") {
                req = req.bearer_auth(api_key);
            }
            let res = req
                .send()
                .await
                .map_err(|e| format!("RAG service request to {rag_url} failed: {e}"))?;

            if !res.status().is_success() {
                return Err(format!("RAG service returned HTTP {}", res.status()));
            }

            let body: serde_json::Value = res
                .json()
                .await
                .map_err(|e| format!("RAG service returned a non-JSON or unexpected response: {e}"))?;

            Ok(AgentToolResult::text(body.to_string()))
        },
    )
}
