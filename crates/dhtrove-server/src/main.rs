mod app;
mod config;

use anyhow::{bail, Context, Result};
use rand::RngCore;
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::PathBuf,
};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

fn admin_token(directory: &std::path::Path) -> Result<String> {
    if let Some(value) = env::var("DHTROVE_API_TOKEN").ok().filter(|v| !v.is_empty()) {
        if value.len() < 16 || value.chars().any(|c| c == '\r' || c == '\n') {
            bail!("DHTROVE_API_TOKEN must be at least 16 characters and contain no line breaks");
        }
        return Ok(value);
    }
    let path = directory.join("admin.token");
    match fs::read_to_string(&path) {
        Ok(value) => {
            let token = value.trim().to_owned();
            if token.len() < 16 {
                bail!("admin.token is invalid or too short");
            }
            return Ok(token);
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let token: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    tracing::info!(path = %path.display(), "generated API token; read this file to sign in");
    Ok(token)
}

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
    let token = admin_token(&directory)?;
    let settings = config::load(&directory.join("settings.json"))?;
    let service = app::Service::new(directory, token, settings);

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
