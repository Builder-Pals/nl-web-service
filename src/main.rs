mod app;
mod archive;
mod config;
mod db;
mod error;
mod model;
mod roblox;
mod transform;

use anyhow::Result;
use config::Config;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .init();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    let state = app::AppState::new(config.clone(), pool).await?;
    let listener = TcpListener::bind(config.bind_address).await?;
    info!(address = %config.bind_address, "server listening");
    axum::serve(listener, app::router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler")
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install signal handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
