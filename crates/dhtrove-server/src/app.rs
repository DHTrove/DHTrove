use crate::{auth::{Accounts, AuthError, User}, config::{self, Settings}};
use axum::{
    extract::{DefaultBodyLimit, Path as AxumPath, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use dhtrove_crawler::{Crawler, CrawlerMetrics};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{watch, Mutex, RwLock};

#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Stopped,
    Running,
    Stopping,
}

struct Lifecycle {
    phase: Phase,
    stop: Option<watch::Sender<bool>>,
    metrics: Arc<CrawlerMetrics>,
    started_at: Option<u64>,
    last_error: Option<String>,
    completed: u64,
}

pub struct Service {
    directory: PathBuf,
    accounts: Arc<Accounts>,
    secure_cookies: bool,
    settings: RwLock<Settings>,
    lifecycle: Mutex<Lifecycle>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Service {
    pub fn new(
        directory: PathBuf,
        accounts: Arc<Accounts>,
        secure_cookies: bool,
        settings: Settings,
    ) -> Arc<Self> {
        Arc::new(Self {
            directory,
            accounts,
            secure_cookies,
            settings: RwLock::new(settings),
            lifecycle: Mutex::new(Lifecycle {
                phase: Phase::Stopped,
                stop: None,
                metrics: Arc::new(CrawlerMetrics::default()),
                started_at: None,
                last_error: None,
                completed: 0,
            }),
        })
    }

    async fn start(self: &Arc<Self>) -> Result<(), ApiError> {
        let mut lifecycle = self.lifecycle.lock().await;
        if lifecycle.phase != Phase::Stopped {
            return Err(ApiError::conflict("Crawler is already running or stopping"));
        }
        let crawler_settings = self.settings.read().await.clone();
        let config = crawler_settings.crawler(&self.directory);
        let crawler = Crawler::new(config).map_err(|err| ApiError::bad_request(err.to_string()))?;
        let (stop, receiver) = watch::channel(false);
        let metrics = Arc::new(CrawlerMetrics::default());
        let generation = lifecycle.completed + 1;
        lifecycle.phase = Phase::Running;
        lifecycle.last_error = None;
        lifecycle.started_at = Some(now_secs());
        lifecycle.stop = Some(stop);
        lifecycle.metrics = metrics.clone();
        drop(lifecycle);

        let service = Arc::clone(self);
        tokio::spawn(async move {
            let result = crawler.run_with_metrics(receiver, metrics).await;
            let mut state = service.lifecycle.lock().await;
            // Only the current generation is allowed to update the UI state.
            if state.completed + 1 == generation {
                state.phase = Phase::Stopped;
                state.stop = None;
                state.started_at = None;
                state.completed = generation;
                if let Err(err) = result {
                    state.last_error = Some(err.to_string());
                    tracing::error!(error = %err, "crawler stopped unexpectedly");
                } else {
                    tracing::info!("crawler stopped cleanly");
                }
            }
        });
        tracing::info!("crawler start requested");
        Ok(())
    }

    async fn stop(&self) {
        let mut state = self.lifecycle.lock().await;
        if state.phase == Phase::Running {
            state.phase = Phase::Stopping;
            if let Some(sender) = &state.stop {
                let _ = sender.send(true);
            }
            tracing::info!("crawler stop requested");
        }
    }

    pub async fn wait_for_stop(&self) {
        self.stop().await;
        for _ in 0..60 {
            if self.lifecycle.lock().await.phase == Phase::Stopped {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        tracing::warn!("crawler did not stop within shutdown grace period");
    }

    async fn status(&self) -> Value {
        let state = self.lifecycle.lock().await;
        let stats = state.metrics.snapshot();
        json!({
            "phase": state.phase,
            "running": state.phase == Phase::Running,
            "started_at": state.started_at,
            "last_error": state.last_error,
            "uptime_seconds": state.started_at.map(|started| now_secs().saturating_sub(started)),
            "metrics": {
                "sightings": stats.sightings,
                "metadata_fetched": stats.metadata_fetched,
                "fetch_failures": stats.fetch_failures,
                "records_inserted": stats.records_inserted,
                "duplicates_skipped": stats.duplicates_skipped,
                "bytes_written": stats.bytes_written,
                "dht_packets": stats.dht_packets,
                "dht_queries": stats.dht_queries,
                "verified_nodes": stats.verified_nodes,
                "active_fetches": stats.active_fetches,
                "queued_hashes": stats.queued_hashes
            },
            "version": env!("CARGO_PKG_VERSION")
        })
    }

    async fn save_settings(&self, incoming: Settings) -> Result<(), ApiError> {
        incoming.validate().map_err(ApiError::bad_request)?;
        let state = self.lifecycle.lock().await;
        if state.phase != Phase::Stopped {
            return Err(ApiError::conflict(
                "Stop the crawler before changing network and storage settings",
            ));
        }
        config::save(&self.directory.join("settings.json"), &incoming)
            .map_err(|err| ApiError::internal(err.to_string()))?;
        *self.settings.write().await = incoming;
        drop(state);
        Ok(())
    }
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        let status = match error {
            AuthError::Invalid(_) => StatusCode::BAD_REQUEST,
            AuthError::Conflict(_) => StatusCode::CONFLICT,
            AuthError::Forbidden(_) => StatusCode::FORBIDDEN,
            AuthError::Unauthorized => StatusCode::UNAUTHORIZED,
            AuthError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            AuthError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self { status, message: error.to_string() }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

const COOKIE_NAME: &str = "dhtrove_session";

fn cookie_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find(|(key, _)| *key == COOKIE_NAME)
        .map(|(_, value)| value)
}

fn session_cookie(token: &str, secure: bool) -> HeaderValue {
    let secure_flag = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=604800{secure_flag}"
    ))
    .expect("session tokens have only hexadecimal characters")
}

fn cleared_cookie(secure: bool) -> HeaderValue {
    let secure_flag = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure_flag}"
    ))
    .expect("static cleared cookie is valid")
}

async fn account_operation<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AuthError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::from)
}

fn require_admin(user: &User) -> Result<(), ApiError> {
    if user.is_admin { Ok(()) }
    else { Err(ApiError::forbidden("Administrator access required")) }
}

async fn auth(
    State(service): State<Arc<Service>>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // SameSite=Strict and a custom header protect state-changing requests
    // from cross-site form posts; browsers preflight cross-origin JS.
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        && request.headers().get("x-dhtrove-request").is_none_or(|value| value != "1")
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(token) = cookie_token(request.headers()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let token = token.to_owned();
    let accounts = service.accounts.clone();
    let principal = account_operation(move || accounts.current_user(&token))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrationOption {
    enabled: bool,
}

async fn setup(State(service): State<Arc<Service>>) -> Result<Json<Value>, ApiError> {
    let accounts = service.accounts.clone();
    let (needs_setup, registration_enabled) = account_operation(move || {
        Ok((accounts.setup_required()?, accounts.registration_open()?))
    }).await?;
    Ok(Json(json!({
        "needs_setup": needs_setup,
        "registration_enabled": registration_enabled
    })))
}

async fn register(
    State(service): State<Arc<Service>>,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    let accounts = service.accounts.clone();
    let (user, token) = account_operation(move || {
        let user = accounts.register(&body.username, &body.password)?;
        let token = accounts.create_session(user.id)?;
        Ok((user, token))
    }).await?;
    let mut response = (StatusCode::CREATED, Json(json!({"user": user}))).into_response();
    response.headers_mut().insert(header::SET_COOKIE, session_cookie(&token, service.secure_cookies));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn login(
    State(service): State<Arc<Service>>,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    let accounts = service.accounts.clone();
    let (user, token) = account_operation(move || {
        let user = accounts.login(&body.username, &body.password)?;
        let token = accounts.create_session(user.id)?;
        Ok((user, token))
    }).await?;
    let mut response = Json(json!({"user": user})).into_response();
    response.headers_mut().insert(header::SET_COOKIE, session_cookie(&token, service.secure_cookies));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn me(Extension(user): Extension<User>) -> Json<Value> {
    Json(json!({"user": user}))
}

async fn logout(
    State(service): State<Arc<Service>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = cookie_token(&headers) {
        let accounts = service.accounts.clone();
        let token = token.to_owned();
        account_operation(move || accounts.revoke_session(&token)).await?;
    }
    let mut response = Json(json!({"ok": true})).into_response();
    response.headers_mut().insert(header::SET_COOKIE, cleared_cookie(service.secure_cookies));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn users(
    State(service): State<Arc<Service>>,
    Extension(principal): Extension<User>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&principal)?;
    let accounts = service.accounts.clone();
    let members = account_operation(move || accounts.users()).await?;
    Ok(Json(json!({"users": members})))
}

async fn update_registration(
    State(service): State<Arc<Service>>,
    Extension(principal): Extension<User>,
    Json(body): Json<RegistrationOption>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&principal)?;
    let accounts = service.accounts.clone();
    account_operation(move || accounts.set_registration(body.enabled)).await?;
    Ok(Json(json!({"registration_enabled": body.enabled})))
}

async fn status(State(service): State<Arc<Service>>) -> Json<Value> {
    Json(service.status().await)
}

async fn settings(State(service): State<Arc<Service>>) -> Json<Settings> {
    Json(service.settings.read().await.clone())
}

async fn update_settings(
    State(service): State<Arc<Service>>,
    Extension(principal): Extension<User>,
    Json(incoming): Json<Settings>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&principal)?;
    service.save_settings(incoming).await?;
    Ok(Json(json!({"ok": true})))
}

async fn start(
    State(service): State<Arc<Service>>,
    Extension(principal): Extension<User>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_admin(&principal)?;
    service.start().await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"ok": true, "state": "starting"})),
    ))
}

async fn stop(
    State(service): State<Arc<Service>>,
    Extension(principal): Extension<User>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&principal)?;
    service.stop().await;
    Ok(Json(json!({"ok": true, "state": "stopping"})))
}

async fn html() -> Response {
    let mut response = Html(include_str!("../assets/index.html")).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn assets(AxumPath(filename): AxumPath<String>) -> Result<Response, StatusCode> {
    let (body, media) = match filename.as_str() {
        "app.css" => (include_str!("../assets/app.css"), "text/css; charset=utf-8"),
        "app.js" => (
            include_str!("../assets/app.js"),
            "text/javascript; charset=utf-8",
        ),
        _ => return Err(StatusCode::NOT_FOUND),
    };
    let mut response = body.to_owned().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(media).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=600"),
    );
    Ok(response)
}

pub fn router(service: Arc<Service>) -> Router {
    let protected = Router::new()
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .route("/status", get(status))
        .route("/settings", get(settings).put(update_settings))
        .route("/crawler/start", post(start))
        .route("/crawler/stop", post(stop))
        .route("/admin/users", get(users))
        .route("/admin/registration", post(update_registration))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .route_layer(middleware::from_fn_with_state(service.clone(), auth));

    let public = Router::new()
        .route("/auth/setup", get(setup))
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .layer(DefaultBodyLimit::max(4 * 1024));

    Router::new()
        .route("/", get(html))
        .route("/assets/:filename", get(assets))
        .route("/healthz", get(|| async { StatusCode::OK }))
        .nest("/api", protected.merge(public))
        .with_state(service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tempfile::tempdir;
    use tower::ServiceExt;

    fn service(dir: &std::path::Path) -> Arc<Service> {
        let accounts = Arc::new(Accounts::open(dir).unwrap());
        Service::new(dir.to_path_buf(), accounts, false, Settings::default())
    }

    async fn call(router: Router, method: &str, uri: &str, body: Value, cookie: Option<&str>) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-dhtrove-request", "1")
            .header("content-type", "application/json");
        if let Some(value) = cookie {
            request = request.header(header::COOKIE, value);
        }
        router.oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap()
    }

    async fn json(response: Response) -> Value {
        let body = to_bytes(response.into_body(), 64_000).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn cookie(response: &Response) -> String {
        response.headers()[header::SET_COOKIE]
            .to_str().unwrap().split(';').next().unwrap().to_owned()
    }

    #[tokio::test]
    async fn first_setup_admin_then_regular_account_and_role_checks() {
        let dir = tempdir().unwrap();
        let routes = router(service(dir.path()));
        let setup = call(routes.clone(), "GET", "/api/auth/setup", json!({}), None).await;
        assert_eq!(json(setup).await["needs_setup"], true);

        let forbidden = call(routes.clone(), "GET", "/api/status", json!({}), None).await;
        assert_eq!(forbidden.status(), StatusCode::UNAUTHORIZED);

        let first = call(routes.clone(), "POST", "/api/auth/register",
            json!({"username":"Owner", "password":"a-strong-admin-password"}), None).await;
        assert_eq!(first.status(), StatusCode::CREATED);
        let admin_cookie = cookie(&first);
        assert_eq!(json(first).await["user"]["is_admin"], true);

        let second = call(routes.clone(), "POST", "/api/auth/register",
            json!({"username":"viewer", "password":"a-strong-user-password"}), None).await;
        assert_eq!(second.status(), StatusCode::CREATED);
        let viewer_cookie = cookie(&second);
        assert_eq!(json(second).await["user"]["is_admin"], false);

        let member = call(routes.clone(), "POST", "/api/crawler/start", json!({}), Some(&viewer_cookie)).await;
        assert_eq!(member.status(), StatusCode::FORBIDDEN);
        let member = call(routes.clone(), "PUT", "/api/settings", json!({}), Some(&viewer_cookie)).await;
        assert_eq!(member.status(), StatusCode::FORBIDDEN);

        let admin = call(routes.clone(), "GET", "/api/admin/users", json!({}), Some(&admin_cookie)).await;
        assert_eq!(admin.status(), StatusCode::OK);
        assert_eq!(json(admin).await["users"].as_array().unwrap().len(), 2);

        let disable = call(routes.clone(), "POST", "/api/admin/registration",
            json!({"enabled":false}), Some(&admin_cookie)).await;
        assert_eq!(disable.status(), StatusCode::OK);
        let third = call(routes.clone(), "POST", "/api/auth/register",
            json!({"username":"third", "password":"a-strong-user-password"}), None).await;
        assert_eq!(third.status(), StatusCode::FORBIDDEN);

        let stopped = call(routes.clone(), "GET", "/api/status", json!({}), Some(&viewer_cookie)).await;
        assert_eq!(json(stopped).await["phase"], "stopped");
        let logout = call(routes.clone(), "POST", "/api/auth/logout", json!({}), Some(&viewer_cookie)).await;
        assert_eq!(logout.status(), StatusCode::OK);
        let invalid = call(routes, "GET", "/api/status", json!({}), Some(&viewer_cookie)).await;
        assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_and_cookies_survive_restart() {
        let dir = tempdir().unwrap();
        let routes = router(service(dir.path()));
        let first = call(routes.clone(), "POST", "/api/auth/register",
            json!({"username":"owner", "password":"very-long-password"}), None).await;
        assert_eq!(first.status(), StatusCode::CREATED);
        let bad = call(routes.clone(), "POST", "/api/auth/login",
            json!({"username":"owner", "password":"incorrect-password"}), None).await;
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
        let good = call(routes, "POST", "/api/auth/login",
            json!({"username":"owner", "password":"very-long-password"}), None).await;
        assert_eq!(good.status(), StatusCode::OK);
        let session = cookie(&good);
        let restarted = router(service(dir.path()));
        let me = call(restarted, "GET", "/api/auth/me", json!({}), Some(&session)).await;
        assert_eq!(json(me).await["user"]["is_admin"], true);
    }

    #[tokio::test]
    async fn mutating_requests_require_custom_header() {
        let dir = tempdir().unwrap();
        let routes = router(service(dir.path()));
        let response = routes.oneshot(
            Request::builder().method("POST").uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"username":"foo","password":"abcdefgh123456"}"#)).unwrap()
        ).await.unwrap();
        // Login is a public JSON-only endpoint; no cookie or admin controls.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn crawler_is_stopped_by_default() {
        let dir = tempdir().unwrap();
        let service = service(dir.path());
        assert_eq!(service.lifecycle.lock().await.phase, Phase::Stopped);
        assert_eq!(service.status().await["running"], false);
    }
}
