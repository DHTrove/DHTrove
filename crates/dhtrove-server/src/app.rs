use crate::config::{self, Settings};
use axum::{
    extract::{DefaultBodyLimit, Path as AxumPath, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use dhtrove_crawler::{Crawler, CrawlerMetrics};
use serde::Serialize;
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
    token: String,
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
    pub fn new(directory: PathBuf, token: String, settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            directory,
            token,
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
        let crawler = Crawler::new(config)
            .map_err(|err| ApiError::bad_request(err.to_string()))?;
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
        Self { status: StatusCode::BAD_REQUEST, message: message.into() }
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self { status: StatusCode::CONFLICT, message: message.into() }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: message.into() }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

fn token_matches(supplied: &[u8], expected: &[u8]) -> bool {
    if supplied.len() != expected.len() { return false; }
    let mut mismatch = 0u8;
    for (&left, &right) in supplied.iter().zip(expected) {
        mismatch |= left ^ right;
    }
    mismatch == 0
}

async fn auth(
    State(service): State<Arc<Service>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = format!("Bearer {}", service.token);
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .is_some_and(|header| token_matches(header.as_bytes(), expected.as_bytes()));
    if !authorized { return Err(StatusCode::UNAUTHORIZED); }
    Ok(next.run(request).await)
}

async fn status(State(service): State<Arc<Service>>) -> Json<Value> {
    Json(service.status().await)
}

async fn settings(State(service): State<Arc<Service>>) -> Json<Settings> {
    Json(service.settings.read().await.clone())
}

async fn update_settings(
    State(service): State<Arc<Service>>,
    Json(incoming): Json<Settings>,
) -> Result<Json<Value>, ApiError> {
    service.save_settings(incoming).await?;
    Ok(Json(json!({"ok": true})))
}

async fn start(State(service): State<Arc<Service>>) -> Result<(StatusCode, Json<Value>), ApiError> {
    service.start().await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"ok": true, "state": "starting"}))))
}

async fn stop(State(service): State<Arc<Service>>) -> Json<Value> {
    service.stop().await;
    Json(json!({"ok": true, "state": "stopping"}))
}

async fn html() -> Response {
    let mut response = Html(include_str!("../assets/index.html")).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'"),
    );
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn assets(AxumPath(filename): AxumPath<String>) -> Result<Response, StatusCode> {
    let (body, media) = match filename.as_str() {
        "app.css" => (include_str!("../assets/app.css"), "text/css; charset=utf-8"),
        "app.js" => (include_str!("../assets/app.js"), "text/javascript; charset=utf-8"),
        _ => return Err(StatusCode::NOT_FOUND),
    };
    let mut response = body.to_owned().into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(media).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=600"));
    Ok(response)
}

pub fn router(service: Arc<Service>) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/settings", get(settings).put(update_settings))
        .route("/crawler/start", post(start))
        .route("/crawler/stop", post(stop))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .route_layer(middleware::from_fn_with_state(service.clone(), auth));
    Router::new()
        .route("/", get(html))
        .route("/assets/:filename", get(assets))
        .route("/healthz", get(|| async { StatusCode::OK }))
        .nest("/api", api)
        .with_state(service)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_check_does_not_accept_prefixes() {
        assert!(token_matches(b"secret", b"secret"));
        assert!(!token_matches(b"sec", b"secret"));
        assert!(!token_matches(b"other!", b"secret"));
    }
    #[tokio::test]
    async fn crawler_is_stopped_by_default() {
        let service = Service::new(PathBuf::from("/does-not-exist"), "secret".into(), Settings::default());
        assert_eq!(service.lifecycle.lock().await.phase, Phase::Stopped);
        assert_eq!(service.status().await["running"], false);
    }
}
