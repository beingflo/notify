//! notify — a mail relay with two doors.
//!
//! `POST /feedback` is public, CORS-restricted to the sites that host a
//! feedback form, and rate limited. `POST /alert` is bearer-token
//! authenticated and not rate limited. Both answer as soon as the message is
//! queued; a single sender task does the SMTP.

mod api;
mod config;
mod deliver;
mod limit;
mod mail;

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

use crate::config::Settings;
use crate::mail::{LogOnly, Mailer, Smtp};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let settings = Settings::from_env().map_err(std::io::Error::other)?;

    let dry_run = settings.smtp_credentials().is_none();
    let mailer: Arc<dyn Mailer> = match settings.smtp_credentials() {
        Some((user, password)) => {
            Arc::new(Smtp::new(user, password).map_err(std::io::Error::other)?)
        }
        None => Arc::new(LogOnly),
    };

    if dry_run {
        tracing::warn!("==========================================================");
        tracing::warn!("DRY RUN, no mail will be sent: {}", settings.why_dry_run());
        tracing::warn!("payloads are logged in full instead of being delivered");
        tracing::warn!("==========================================================");
    }

    // The sender starts before the server does, so the first request has
    // somewhere to put its mail.
    let (queue, receiver) = mpsc::channel(config::QUEUE_CAPACITY);
    let sender = tokio::spawn(deliver::run(receiver, mailer, dry_run));

    let listener = TcpListener::bind(("0.0.0.0", settings.serve_port)).await?;
    tracing::info!(
        port = settings.serve_port,
        dry_run,
        alerts = settings.alert_token.is_some(),
        "notify: listening"
    );

    // The match arm's future owns the router, so finishing it drops the last
    // queue sender and closes the channel: that is what ends the drain below.
    let served = {
        let app = api::router(api::AppState::new(queue, settings.alert_token, dry_run));
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    };

    match timeout(config::DRAIN_TIMEOUT, sender).await {
        Ok(Ok(())) => tracing::info!("drained"),
        Ok(Err(error)) => tracing::error!(%error, "sender task panicked"),
        Err(_) => tracing::error!(
            seconds = config::DRAIN_TIMEOUT.as_secs(),
            "drain deadline passed; queued mail still unsent is lost"
        ),
    }

    // A bind failure is the only error `serve` can report, and it is worth
    // failing on: the unit's restart policy will try again.
    served?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT received, shutting down"),
        _ = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}
