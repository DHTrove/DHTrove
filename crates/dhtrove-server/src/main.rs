mod app;
mod auth;
mod config;

use anyhow::{Context, Result};
use std::sync::Arc;
use std::{env, fs, net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let directory =
        PathBuf::from(env::var("DHTROVE_DATA_DIR").unwrap_or_else(|_| "./dhtrove-data".into()));
    fs::create_dir_all(&directory).context("create data directory")?;
    let accounts = Arc::new(auth::Accounts::open(&directory).context("open user database")?);
    let settings = config::load(&directory.join("settings.json"))?;
    // Cookies must use Secure when the browser-facing URL is HTTPS. For
    // loopback HTTP development this remains false by default.
    let secure_cookies = env::var("DHTROVE_SECURE_COOKIES")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
    let service = app::Service::new(directory, accounts, secure_cookies, settings);

    // Starting this process launches the API only. The DHT crawler is
    // explicitly stopped until the user presses Start or calls POST /api/crawler/start.
    let listen: SocketAddr = env::var("DHTROVE_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()
        .context("DHTROVE_BIND must be a socket address")?;
    let listener = TcpListener::bind(listen)
        .await
        .context("bind HTTP listener")?;
    tracing::info!(address = %listener.local_addr()?, "web dashboard started; crawler is STOPPED");

    let shutdown_service = service.clone();
    axum::serve(listener, app::router(service))
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                if let Ok(mut terminate) = signal(SignalKind::terminate()) {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = terminate.recv() => {}
                    };
                } else {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            shutdown_service.wait_for_stop().await;
        })
        .await
        .context("HTTP server exited with error")?;
    Ok(())
}
