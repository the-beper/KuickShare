mod allowed;
mod app;
mod consent;
mod dbus;
mod history;
mod link;
mod persist;
mod ratelimit;
mod requirements;
mod safety;
mod settings;
mod trust;
mod wifi_credentials;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use crate::{
    app::Core,
    history::History,
    requirements::{EngineConfig, SystemEngine, SystemRequirementsProbe},
    settings::Settings,
    trust::Trust,
};

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
    // The Wi-Fi requirements: one engine for the report and for the gate on the upgrade setting.
    let requirements = SystemEngine::new(SystemRequirementsProbe::new(), EngineConfig::default());
    core.lock().await.set_requirements(requirements.clone());
    let consents = core.lock().await.consents();
    // Profiles a crash left behind (possibly in an open firewall zone) go first, whatever the
    // settings say, and before the bus name is taken: no request sees the service half-started.
    // Kept for the exit: the cleanup there must not wait for the `Core` lock.
    let release = core.lock().await.wifi_release();
    app::release_wifi(release.clone(), app::WIFI_START_SWEEP_BOUND).await;
    let connection = dbus::serve(core.clone(), consents, requirements.clone()).await?;
    core.lock().await.start(connection).await?;
    requirements::watch::spawn(requirements);
    tokio::spawn(app::watch_network(core.clone()));

    // The shell stops the daemon with SIGTERM; Ctrl+C is for running it by hand.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    tracing::info!("stopping");
    // Engine at most 0.8 s, then the Wi-Fi cleanup at most 2 s, whatever the engine does or
    // whoever holds the `Core` lock: both within the shell's 3 s wait after SIGTERM. The cleanup
    // closes the firewall port and restores the connection a hotspot replaced, on the shared link.
    app::stop_daemon(
        async { core.lock().await.shutdown().await },
        app::SHUTDOWN_ENGINE_BOUND,
        app::release_wifi(release, app::WIFI_STOP_CLEANUP_BOUND),
    )
    .await;
    Ok(())
}
