//! The HTTP API: routes and handlers, the SSE event stream, and the
//! API token guard in front of them.

use super::*;

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/api/sessions", post(create_session).get(list_sessions))
        // New (F-12): rename / delete a session.
        .route("/api/sessions/{id}", patch(patch_session_route).delete(delete_session_route))
        .route("/api/sessions/{id}/messages", get(get_messages))
        .route("/api/sessions/{id}/prompt", post(prompt))
        .route("/api/sessions/{id}/abort", post(abort_session))
        // New (F-12): plan/build mode, model, permission mode, waiting approvals.
        .route("/api/sessions/{id}/mode", get(get_mode_route).put(put_mode_route))
        .route("/api/model", get(get_model_route).put(put_model_route))
        .route("/api/permission-mode", get(get_perm_mode_route).put(put_perm_mode_route))
        .route("/api/approvals", get(get_approvals_route))
        .route("/api/approvals/{id}", post(decide_approval))
        .route("/api/events", get(events_sse))
        .with_state(state)
}

#[derive(Deserialize)]
pub(super) struct CreateSessionReq {
    #[serde(default)]
    pub(super) title: String,
}

pub(super) async fn create_session(
    State(st): State<Shared>,
    Json(req): Json<CreateSessionReq>,
) -> Result<Json<Session>, ApiError> {
    let title = if req.title.is_empty() { "new session".to_string() } else { req.title };
    let s = st.db.run(move |db| db.create_session(&title, None)).await.map_err(internal)?;
    let _ = st.events.send(ServerEvent::SessionCreated { session_id: s.id.clone(), title: s.title.clone() });
    Ok(Json(s))
}

pub(super) async fn list_sessions(State(st): State<Shared>) -> Result<Json<Vec<Session>>, ApiError> {
    Ok(Json(st.db.run(|db| db.list_sessions()).await.map_err(internal)?))
}

/// T-001: does chat `id` exist? Asked on the blocking pool, like every
/// database call of a handler (SQLite blocks; a tokio worker must not).
pub(super) async fn session_exists(st: &Shared, id: &str) -> Result<bool, ApiError> {
    let id = id.to_string();
    st.db.run(move |db| Ok(db.get_session(&id)?.is_some())).await.map_err(internal)
}

pub(super) async fn get_messages(
    State(st): State<Shared>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Vec<Message>>, ApiError> {
    let messages = st
        .db
        .run(move |db| {
            Ok(match db.get_session(&id)? {
                Some(_) => Some(db.messages(&id)?),
                None => None,
            })
        })
        .await
        .map_err(internal)?;
    messages.map(Json).ok_or_else(session_not_found)
}

#[derive(Deserialize)]
pub(super) struct PromptReq {
    pub(super) content: String,
}

/// Kick off an agent run for a user message. Returns 202 immediately;
/// progress arrives over SSE. Replaces any current run (with abort).
pub(super) async fn prompt(
    State(st): State<Shared>,
    AxPath(id): AxPath<String>,
    Json(req): Json<PromptReq>,
) -> Result<StatusCode, ApiError> {
    if !session_exists(&st, &id).await? {
        return Err(session_not_found());
    }
    if req.content.trim().is_empty() {
        return Err(ApiError::bad_request("content is empty"));
    }
    // Replaces (aborts) any run already active on this session.
    st.kick_run(id, req.content).await;
    Ok(StatusCode::ACCEPTED)
}

pub(super) async fn abort_session(
    State(st): State<Shared>,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let busy = st.busy.read().await;
    if let Some(flag) = busy.get(&id) {
        signal_abort(flag);
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError::not_found("no run is active in this session"))
    }
}

#[derive(Deserialize)]
pub(super) struct DecideReq {
    pub(super) allow: bool,
    #[serde(default)]
    pub(super) always: bool,
}

pub(super) async fn decide_approval(
    State(st): State<Shared>,
    AxPath(id): AxPath<String>,
    Json(req): Json<DecideReq>,
) -> Result<StatusCode, ApiError> {
    if st.decide(&id, req.allow, req.always) {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError::not_found("approval not found (already answered or timed out)"))
    }
}

#[derive(Deserialize)]
pub(super) struct ModeReq {
    pub(super) mode: String,
}

/// New (F-12): `GET /api/sessions/{id}/mode`.
pub(super) async fn get_mode_route(
    State(st): State<Shared>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Value>, ApiError> {
    if !session_exists(&st, &id).await? {
        return Err(session_not_found());
    }
    Ok(Json(json!({ "mode": st.mode_for(&id).as_str() })))
}

/// New (F-12): `PUT /api/sessions/{id}/mode`.
pub(super) async fn put_mode_route(
    State(st): State<Shared>,
    AxPath(id): AxPath<String>,
    Json(req): Json<ModeReq>,
) -> Result<Json<Value>, ApiError> {
    if !session_exists(&st, &id).await? {
        return Err(session_not_found());
    }
    let mode = AgentMode::parse(&req.mode).ok_or_else(|| ApiError::bad_request("unknown mode: use plan or build"))?;
    st.set_mode(&id, mode);
    Ok(Json(json!({ "mode": mode.as_str() })))
}

#[derive(Deserialize)]
pub(super) struct ModelReq {
    pub(super) model: String,
}

/// New (F-12): `GET /api/model`. `available` is empty when the provider
/// cannot list its models.
pub(super) async fn get_model_route(State(st): State<Shared>) -> Json<Value> {
    Json(json!({
        "current": st.current_model().await,
        "available": st.available_models().await,
    }))
}

/// New (F-12): `PUT /api/model`.
pub(super) async fn put_model_route(
    State(st): State<Shared>,
    Json(req): Json<ModelReq>,
) -> Result<Json<Value>, ApiError> {
    let model = req.model.trim();
    if model.is_empty() {
        return Err(ApiError::bad_request("model is empty"));
    }
    st.set_model(model).await;
    Ok(Json(json!({ "current": st.current_model().await })))
}

/// New (F-12): `GET /api/permission-mode`.
pub(super) async fn get_perm_mode_route(State(st): State<Shared>) -> Json<Value> {
    Json(json!({ "mode": st.perm_mode().as_str() }))
}

/// New (F-12): `PUT /api/permission-mode`. `released` lists the approvals
/// that turbo answered on the spot.
pub(super) async fn put_perm_mode_route(
    State(st): State<Shared>,
    Json(req): Json<ModeReq>,
) -> Result<Json<Value>, ApiError> {
    let mode = PermissionMode::parse(&req.mode)
        .ok_or_else(|| ApiError::bad_request("unknown permission mode: use default, manual or turbo"))?;
    let released = st.set_perm_mode(mode);
    Ok(Json(json!({ "mode": mode.as_str(), "released": released })))
}

/// New (F-12): `GET /api/approvals` — every approval still waiting, as the
/// same `ApprovalNeeded` events the event stream sent.
pub(super) async fn get_approvals_route(State(st): State<Shared>) -> Json<Vec<ServerEvent>> {
    Json(st.pending_approvals())
}

#[derive(Deserialize)]
pub(super) struct RenameReq {
    pub(super) title: String,
}

/// New (F-12): `PATCH /api/sessions/{id}` — rename.
pub(super) async fn patch_session_route(
    State(st): State<Shared>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RenameReq>,
) -> Result<Json<Session>, ApiError> {
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(ApiError::bad_request("title is empty"));
    }
    // Check, rename and read back in one trip to the blocking pool.
    let renamed = st
        .db
        .run(move |db| {
            if db.get_session(&id)?.is_none() {
                return Ok(None);
            }
            db.set_session_title(&id, &title)?;
            db.get_session(&id)
        })
        .await
        .map_err(internal)?;
    renamed.map(Json).ok_or_else(session_not_found)
}

/// New (F-12): `DELETE /api/sessions/{id}`. A running run is stopped first,
/// and the session is deleted only once that run has ended, so nothing is
/// written to a deleted session. 409 when the run does not end in time
/// ([`RUN_STOP_WAIT_SECS`]).
pub(super) async fn delete_session_route(
    State(st): State<Shared>,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    if !session_exists(&st, &id).await? {
        return Err(session_not_found());
    }
    if st.abort_run(&id).await {
        // Woken when a run lets go of its slot (no polling).
        let wait = async {
            loop {
                let released = run_released().notified();
                tokio::pin!(released);
                released.as_mut().enable();
                if !st.run_active(&id).await {
                    return;
                }
                released.await;
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(RUN_STOP_WAIT_SECS), wait).await.is_err() {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("the run did not stop within {RUN_STOP_WAIT_SECS} s; try again"),
            ));
        }
    }
    let sid = id.clone();
    if st.db.run(move |db| db.delete_session(&sid)).await.map_err(internal)? {
        st.forget_session(&id);
        // T-028: todos live in their own table with no foreign key.
        let (todos, sid) = (st.clone(), id.clone());
        let forgot = tokio::task::spawn_blocking(move || todos.todo_store.forget(&sid)).await;
        if let Ok(Err(e)) = forgot {
            core_log(&format!("[core] deleting the todos of {id} failed: {e:#}"));
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(session_not_found())
    }
}

pub(super) async fn events_sse(State(st): State<Shared>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = st.events.subscribe();
    // New (B-09): events waiting to be sent before the next live one.
    let queue: std::collections::VecDeque<Event> = std::collections::VecDeque::new();
    let stream = futures::stream::unfold((rx, queue, st), |(mut rx, mut queue, st)| async move {
        loop {
            if let Some(ev) = queue.pop_front() {
                return Some((Ok(ev), (rx, queue, st)));
            }
            match rx.recv().await {
                Ok(ev) => {
                    if let Ok(data) = serde_json::to_string(&ev) {
                        return Some((Ok(Event::default().data(data)), (rx, queue, st)));
                    }
                }
                // New (B-09): this client fell behind and lost events. Say so
                // (SSE event "lagged", data = how many) and send every
                // approval that still waits, so no prompt is lost.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    queue.push_back(Event::default().event("lagged").data(n.to_string()));
                    for ev in st.pending_approvals() {
                        if let Ok(data) = serde_json::to_string(&ev) {
                            queue.push_back(Event::default().data(data));
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub(super) fn internal<E: std::fmt::Display>(e: E) -> ApiError {
    // New (B-48): to the log file, not stderr (stderr draws over the TUI).
    core_log(&format!("[core] internal error: {e}"));
    // The details stay in the log: they can name files and queries.
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
}

/// How long DELETE waits for a session's run to stop before it answers 409
/// (a short wait under test, so the 409 test does not take 10 s).
pub(super) const RUN_STOP_WAIT_SECS: u64 = if cfg!(test) { 3 } else { 10 };

/// T-051: a failed API request: its status, and `{"error": "..."}` saying
/// why, so a client can show more than a bare code.
#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub(super) fn not_found(why: &str) -> Self {
        Self(StatusCode::NOT_FOUND, why.into())
    }

    pub(super) fn bad_request(why: &str) -> Self {
        Self(StatusCode::BAD_REQUEST, why.into())
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

pub(super) fn session_not_found() -> ApiError {
    ApiError::not_found("session not found")
}

/// Health endpoint for readiness checks + startup timing.
pub(super) async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

/// New (S-01): environment variable that fixes the API token. When it is
/// not set, every start makes a new random token (see `resolve_api_token`).
pub const API_TOKEN_ENV: &str = "OPENCODE_RUST_API_TOKEN";

/// New (S-01): what every HTTP request must match (built once in `serve`).
pub(super) struct ApiGuard {
    /// Expected value after `Authorization: Bearer `.
    pub(super) token: String,
    /// The port we listen on; the `Host` header must name it.
    pub(super) port: u16,
}

/// New (S-01): where a generated token is written: `<state_dir>/api-token`.
pub fn api_token_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("api-token")
}

/// New (S-01): one line for the startup output that says where the token
/// is. It never contains the token itself.
pub fn api_token_note(state_dir: &std::path::Path) -> String {
    match std::env::var(API_TOKEN_ENV) {
        Ok(v) if !v.trim().is_empty() => format!("API token: from ${API_TOKEN_ENV}"),
        _ => format!("API token file: {}", api_token_path(state_dir).display()),
    }
}

/// New (S-01): the token for this start: `$OPENCODE_RUST_API_TOKEN` when
/// set, else a fresh random token written to `<state_dir>/api-token`
/// (on Linux/macOS only your user can read that file).
pub(super) fn resolve_api_token(state_dir: &std::path::Path) -> Result<String> {
    if let Ok(v) = std::env::var(API_TOKEN_ENV) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    let token = random_token()?;
    std::fs::create_dir_all(state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let path = api_token_path(state_dir);
    // Remove the old file first: on Unix the mode below only applies to a
    // file that is newly created.
    let _ = std::fs::remove_file(&path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path).with_context(|| format!("writing {}", path.display()))?;
    // Windows has no mode bits: the file inherits the folder's ACL. Give it
    // its own, with the current user only, before the token goes in.
    #[cfg(windows)]
    if let Err(e) = owner_only_acl(&path) {
        // Best effort, like the mode on a filesystem without permissions
        // (FAT); $OPENCODE_RUST_API_TOKEN avoids the file altogether.
        core_log(&format!("[core] could not restrict {}: {e:#}", path.display()));
    }
    std::io::Write::write_all(&mut file, token.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok(token)
}

/// New (S-01): a 256-bit random token as 64 hex characters, straight from
/// the operating system's CSPRNG (it was SipHash over the time and the
/// process id, which is guessable).
pub(super) fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("no random source for the API token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Replace `path`'s ACL with one entry: full access for the current user,
/// nothing inherited from the folder.
#[cfg(windows)]
pub(super) fn owner_only_acl(path: &std::path::Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
        SetFileSecurityW, SetSecurityDescriptorDacl, TokenUser, ACL, ACL_REVISION, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    /// FILE_ALL_ACCESS (the Storage feature that names it is not enabled).
    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    // SAFETY: plain Win32 calls. Every buffer is sized as the call asks
    // (TOKEN_USER through the returned length, the ACL from the SID's
    // length) and outlives the calls that use it; the token handle is
    // closed once.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).context("opening the process token")?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        // u64 cells: TOKEN_USER holds pointers, so the buffer must be aligned.
        let mut user = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let got = GetTokenInformation(token, TokenUser, Some(user.as_mut_ptr().cast()), len, &mut len);
        let _ = CloseHandle(token);
        got.context("reading the current user")?;
        let sid = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
        let acl_len = std::mem::size_of::<ACL>() + 16 + GetLengthSid(sid) as usize;
        let mut acl = vec![0u64; acl_len.div_ceil(8)];
        let acl_ptr = acl.as_mut_ptr() as *mut ACL;
        InitializeAcl(acl_ptr, acl_len as u32, ACL_REVISION).context("building the ACL")?;
        AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, sid).context("building the ACL")?;
        let mut sd = SECURITY_DESCRIPTOR::default();
        let psd = PSECURITY_DESCRIPTOR(&mut sd as *mut _ as *mut std::ffi::c_void);
        InitializeSecurityDescriptor(psd, 1).context("building the security descriptor")?;
        SetSecurityDescriptorDacl(psd, true, Some(acl_ptr), false).context("building the security descriptor")?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        if !SetFileSecurityW(PCWSTR(wide.as_ptr()), info, psd).as_bool() {
            return Err(std::io::Error::last_os_error()).context("setting the file's ACL");
        }
    }
    Ok(())
}

/// New (S-01): the `Host` header must be `127.0.0.1:<port>` or
/// `localhost:<port>`. A web page from another site that points its own
/// domain name at 127.0.0.1 still sends that domain name, so it is refused.
pub(super) fn host_allowed(headers: &axum::http::HeaderMap, port: u16) -> bool {
    let Some(host) = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = host.trim().to_ascii_lowercase();
    let (name, host_port) = match host.rsplit_once(':') {
        Some((name, p)) => (name.to_string(), p.parse::<u16>().ok()),
        None => (host.clone(), Some(80)),
    };
    (name == "127.0.0.1" || name == "localhost") && host_port == Some(port)
}

/// New (S-01): compare two secrets without stopping at the first
/// difference (the time taken does not reveal how much matched).
pub(super) fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// New (S-01): middleware in front of every route (see `serve`).
pub(super) async fn require_token(
    State(guard): State<Arc<ApiGuard>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !host_allowed(req.headers(), guard.port) {
        return (StatusCode::FORBIDDEN, "forbidden: unexpected Host header").into_response();
    }
    if req.uri().path() != "/api/health" {
        let presented = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .trim();
        if !same_secret(presented.as_bytes(), guard.token.as_bytes()) {
            return (
                StatusCode::UNAUTHORIZED,
                [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized: send the header Authorization: Bearer <token>",
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Bind and serve. Returns the actual address (useful with port 0).
/// New (S-01): every route except `/api/health` needs the header
/// `Authorization: Bearer <token>`, and every request must be addressed to
/// `127.0.0.1:<port>` or `localhost:<port>` (the `Host` header).
pub async fn serve(state: Shared, port: u16) -> Result<std::net::SocketAddr> {
    let token = resolve_api_token(&state.state_dir)?;
    serve_with_token(state, port, token).await
}

/// New (S-01): `serve` with a token you choose (tests use this).
pub async fn serve_with_token(state: Shared, port: u16, token: String) -> Result<std::net::SocketAddr> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let guard = Arc::new(ApiGuard { token, port: addr.port() });
    let app = router(state)
        .route("/api/health", get(health))
        .layer(axum::middleware::from_fn_with_state(guard, require_token));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(addr)
}
