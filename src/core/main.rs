mod allowed;
mod app;
mod dbus;
mod history;
mod link;
mod ratelimit;
mod safety;
mod settings;
mod trust;
mod wifi_credentials;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use crate::{app::Core, history::History, settings::Settings, trust::Trust};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,mdns_sd=error,bluez_async=error")),
        )
        .init();

    let settings_path = settings::default_path();
    let settings = settings_path
        .as_deref()
        .map(Settings::load)
        .unwrap_or_default();

    let history = History::load(history::default_path());
    let (trust_tx, trust_rx) = tokio::sync::mpsc::unbounded_channel();
    let device_name = settings
        .device_name
        .clone()
        .unwrap_or_else(settings::default_device_name);
    let trust = Trust::load(trust::default_dir().as_deref(), device_name, trust_tx)?;
    let core = Arc::new(Mutex::new(Core::new(
        settings,
        settings_path,
        history,
        trust,
        trust_rx,
    )));
    core.lock().await.set_handle(Arc::downgrade(&core));
    let lib_actions = core.lock().await.lib_actions();
    let connection = dbus::serve(core.clone(), lib_actions).await?;
    core.lock().await.start(connection).await?;
    tokio::spawn(app::watch_network(core.clone()));

    // The shell stops the daemon with SIGTERM; Ctrl+C is for running it by hand.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    tracing::info!("stopping");
    core.lock().await.shutdown().await;
    Ok(())
}
