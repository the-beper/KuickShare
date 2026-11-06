mod allowed;
mod app;
mod consent;
mod dbus;
mod desktop;
mod history;
mod i18n;
mod link;
mod notify;
mod persist;
mod ratelimit;
mod requirements;
mod safety;
mod settings;
mod tray;
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
    // Profiles a crash left behind (possibly in an open firewall zone) go first, whatever the
    // settings say, and before the bus name is taken: no request sees the service half-started.
    // Kept for the exit: the cleanup there must not wait for the `Core` lock.
    let release = core.lock().await.wifi_release();
    app::release_wifi(release.clone(), app::WIFI_START_SWEEP_BOUND).await;
    // `quit_rx` tells the shutdown that `Quit` came on the bus, `desktop_quit` that the user
    // quit through the tray. The connection is kept: the Notifier and the desktop tasks took
    // their own clones.
    let (quit_tx, mut quit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut desktop_quit = {
        // The desktop legs (tray, notifications, Do Not Disturb) read the merged transfer
        // events of the engine through a channel, the way the shell read `TransferUpdated`.
        let (transfer_tx, transfer_rx) = tokio::sync::mpsc::unbounded_channel();
        core.lock().await.set_desktop(transfer_tx);
        // A fresh book for the desktop to answer prompts with: `serve` takes a clone of it.
        let consents = core.lock().await.consents();
        let connection = dbus::serve(
            core.clone(),
            consents.clone(),
            requirements.clone(),
            quit_tx,
        )
        .await?;
        core.lock().await.start(connection.clone()).await?;
        let notifier = notify::Notifier::new(connection.clone());
        desktop::spawn(core.clone(), connection, consents, notifier, transfer_rx)
    };
    // The pid file tells the shell which detached core is there (see `CoreBridge::startService`,
    // which may have to kill a stuck one). Aged only while the bus name is held.
    pidfile::write();
    requirements::watch::spawn(requirements);
    tokio::spawn(app::watch_network(core.clone()));

    // The shell stops the daemon with SIGTERM; Ctrl+C is for running it by hand. The tray's
    // Quit ends through the same shutdown (see `desktop::spawn`).
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
        _ = quit_requested(&mut desktop_quit) => {}
        // `Quit` on the bus: the shell stops the service before it quits itself.
        _ = quit_requested(&mut quit_rx) => {}
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
    // After the bounded stop: a shutdown that hangs and gets killed leaves the pid for the
    // shell's `killStaleCore`.
    pidfile::remove();
    Ok(())
}

/// Resolves when somebody asks the daemon to quit. The end of the channel is no request (the
/// desktop ended, say because its tray failed): that parks for good, so the `select` it is in
/// keeps waiting for the signals and the other request, and nothing stops a transfer.
async fn quit_requested(quit: &mut tokio::sync::mpsc::UnboundedReceiver<()>) {
    if quit.recv().await.is_none() {
        tracing::error!("a source of quit requests ended; only the others can stop the daemon");
        std::future::pending::<()>().await;
    }
}

/// The pid file the core keeps while it serves, and the shell kills a stuck service by
/// (`CoreBridge::startService`). Same directory the shell reads
/// (`QStandardPaths::CacheLocation`), so one `${XDG_CACHE_HOME}/kuickshare/kuickshared.pid`.
/// Only a process of the same pid-namespace can use it: the shell checks the process name
/// before it kills (`killStaleCore`), so inside a Flatpak nothing is killed, a fresh core
/// starts instead.
mod pidfile {
    use std::fs;
    use std::path::PathBuf;

    /// The file name the shell knows (see `corebridge.cpp`).
    const NAME: &str = "kuickshared.pid";

    fn path() -> Option<PathBuf> {
        let dir = dirs::cache_dir()?.join("kuickshare");
        fs::create_dir_all(&dir).ok()?;
        Some(dir.join(NAME))
    }

    /// Kept while the bus name is held; only a live core serves (see `remove`).
    pub fn write() {
        let Some(path) = path() else {
            return;
        };
        if let Err(e) = fs::write(&path, format!("{}\n", std::process::id())) {
            tracing::debug!("cannot write {}: {e}", path.display());
        }
    }

    /// The shutdown path; a crash leaves a stale pid behind, which the shell verifies by
    /// process name before it does anything (see `write`).
    pub fn remove() {
        if let Some(path) = path()
            && let Err(e) = fs::remove_file(&path)
        {
            tracing::debug!("cannot remove {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::quit_requested;

    #[tokio::test]
    async fn a_request_quits() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), quit_requested(&mut rx))
            .await
            .expect("a request resolves");
    }

    #[tokio::test]
    async fn the_end_of_the_channel_is_not_a_request() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        drop(tx);
        let waited =
            tokio::time::timeout(Duration::from_millis(100), quit_requested(&mut rx)).await;
        assert!(waited.is_err(), "the closed channel must keep waiting");
    }
}
