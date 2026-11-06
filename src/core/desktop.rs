//! The desktop legs of the core: the tray icon ([`crate::tray`]), the notifications
//! ([`crate::notify`]), Do Not Disturb, the clipboard and the way the window and the files
//! open. The background is headless: the core runs alone, and the shell only exists while the
//! user works with its window.
//!
//! The state machine ([`Desktop::run`]) watches the transfers of the engine (already merged,
//! see `forward_transfers`), the presses on the notifications, the tray clicks and a slow tick,
//! and decides the prompts for consents, the notices for how a transfer ended, and the tray
//! state.
//!
//! The texts come from the catalogs of `po/<lang>.po`, which the shell shares.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use futures_util::StreamExt;
use ksni::TrayMethods;
use tokio::{
    io::AsyncWriteExt,
    process,
    sync::{Mutex, mpsc, watch},
};
use zbus::{Connection, Fd, Variant, fdo};

use crate::{
    app::Core,
    consent::{Answer, By, ConsentError, Consents},
    dbus::TransferEvent,
    history,
    notify::{
        self, ACCEPT, ACCEPT_AND_TRUST, COPY, DECLINE, Event, Notice, Notifier, OPEN_FILE,
        OPEN_FOLDER, OPEN_LINK, VIEW,
    },
    tray,
};

/// The state of an incoming transfer waiting for the user's consent.
const WAITING: &str = "WaitingForUserConsent";

/// How often the tray state and its tooltip are refreshed, and the missing prompts looked for.
const PULSE: Duration = Duration::from_secs(2);

/// How long the state of an ended transfer is kept: a repeat of its end (`Disconnected` after
/// `Finished`) arrives within seconds and must find it, and an id that comes back later is a
/// new transfer.
const FINISHED_KEEP: Duration = Duration::from_secs(60);

/// The name the shell owns on the bus while its window lives.
const SHELL_NAME: &str = "org.kde.kuickshare";

/// The duration of the "Visible for 10 Minutes" menu entry, in seconds.
const TEMP_VISIBLE_SECS: u32 = 600;

/// Sees whether the process runs in a Flatpak sandbox: several pieces of the desktop must go
/// through portals there.
const FLATPAK_INFO: &str = "/.flatpak-info";

/// The object path of the Flatpak portal, which starts the shell of this app in the sandbox.
const PORTAL_PATH: &str = "/org/freedesktop/portal/Flatpak";

/// A transfer that is moving data.
fn is_transferring(state: &str) -> bool {
    state == "ReceivingFiles" || state == "SendingFiles"
}

/// Whether the transfer id names a Bluetooth session (`connect_ble` in the library): the
/// "Not Accepted" notice of one asks for Bluetooth-specific advice.
fn is_ble(id: &str) -> bool {
    id.starts_with("ble:")
}

/// Whether the event is a transfer waiting for its user's answer: what the prompt and its
/// re-show go by.
fn waits_consent(event: &TransferEvent) -> bool {
    event.kind == "inbound"
        && event.state == WAITING
        && event.trusted.is_empty()
        && event.consent != 0
}

/// Why an offer was refused by the library, in the words `CoreBridge::refusalText` had.
fn refusal_text(reason: &str) -> String {
    match reason {
        "too_many_files" => crate::i18n::i18n!("The offer has too many files"),
        "not_enough_space" => {
            crate::i18n::i18n!("There is not enough free space in the download folder")
        }
        "destination" => crate::i18n::i18n!("The download folder cannot be used"),
        "bad_name" => crate::i18n::i18n!("A file name in the offer cannot be saved"),
        "bad_size" | "duplicate_id" => crate::i18n::i18n!("The offer is malformed"),
        // The offer may be refused without a reason in this version, or the reason is a word
        // unknown to it.
        "" => String::default(),
        _ => crate::i18n::i18n!("The offer was refused"),
    }
}

/// The minutes left of a temporary visibility, for the tooltip of the tray (rounded up).
fn minutes_left(until: i64, now: i64) -> u64 {
    if until <= now {
        return 0;
    }
    let left = until - now;
    (left / 60 + i64::from(left % 60 > 0)) as u64
}

/// `file://` of a local path: path characters that may not travel inside a URI are escaped, and
/// the rest (letters, numbers, marks, punctuation of a path and URI delimiters) travels as it
/// is. The OpenURI portal and `xdg-open` read it as a host path.
fn file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for byte in path.as_bytes() {
        match byte {
            b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@'
            | b'/' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The AppImage of this run, or `None` when this run is not one. `APPDIR` and `APPIMAGE` must
/// agree, as the shell's `Launcher` measured (the image exists, and this binary lives inside
/// the image directory).
fn app_image() -> Option<PathBuf> {
    let image = std::env::var_os("APPIMAGE").filter(|image| !image.is_empty())?;
    if !Path::new(&image).exists() {
        return None;
    }
    let appdir = std::env::var_os("APPDIR").filter(|dir| !dir.is_empty() && dir != ".")?;
    let appdir = PathBuf::from(appdir);
    std::env::current_exe()
        .ok()
        .filter(|exe| exe.starts_with(&appdir))
        .map(|_| PathBuf::from(image))
}

/// The shell binary which the desktop actions open (`kuickshare`, next to this `kuickshared`),
/// or the running AppImage itself. `None` when the layout says nothing about it: the caller
/// logs that instead of opening a half-known path.
fn shell_program() -> Option<PathBuf> {
    if let Some(image) = app_image() {
        return Some(image);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|bin| bin.join("kuickshare")))
}

/// The folder received files are in, for the "Open Folder" action of a notice: the setting, or
/// the usual download folder of the user.
fn download_dir(core: &Core) -> String {
    if let Some(dir) = &core.settings().download_dir {
        return dir.to_string_lossy().into_owned();
    }
    dirs::download_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// What one open notification answers. The server id maps to this, and the pressed key to the
/// words of the choice. One entry per notice: a re-shown prompt replaces its entry.
enum Button {
    Consent { id: String, token: u64 },
    Text { id: String, text: String },
    File { dir: String, path: String },
    Folder { dir: String },
}

/// The state the notifications keep per transfer id: from the first event until a short while
/// after the end (see [`FINISHED_KEEP`]), so the repeats of an end are told from a new transfer.
#[derive(Default)]
struct Tracked {
    /// The latest event of the transfer, which still says (a) what waits to be answered and
    /// (b) what a late press re-shows for.
    event: TransferEvent,
    /// Server id of the prompt of the open consent; `None` while none shows.
    prompt: Option<u32>,
    /// The token of the consent the prompt answers: another one under the same id is another
    /// transfer, and its prompt replaces this one.
    token: u64,
    /// The core could not trust the sender (no known network address, an unknown name, or an
    /// address taken by another device): the prompt does not offer the choice again.
    no_trust: bool,
    /// The user dismissed the prompt (a click on the body, ...): the consent still waits, but
    /// nothing re-shows it on its own until another consent may take the id.
    dismissed: bool,
    /// Data started to move: a failure notice is only "not accepted" while nothing moved.
    started: bool,
    /// When the transfer ended, whatever its end said (a silent end counts too); the same id may
    /// come back for another transfer later.
    finished: Option<Instant>,
}

/// What the consent part of [`Desktop::on_transfer`] wants to do with the open prompt.
enum PromptStep {
    /// The consent waits and a prompt answers it already.
    Keep,
    /// A prompt of an older consent (the id came back) is there: replace it.
    Replace(u32),
    /// Nothing shows for this consent: make a prompt.
    Show,
    /// The user dismissed this consent's prompt and it is the same consent: leave it off.
    Dismissed,
    /// The transfer does not wait for an answer: drop the prompt.
    Leave(u32),
}

/// What the desktop task owns and decides on.
pub struct Desktop {
    /// The core, for the settings, the visibility, and the window flag.
    core: Arc<Mutex<Core>>,
    /// The session bus, for the notifications (through [`Notifier`]) and the portals.
    connection: Connection,
    /// The client of the notifications daemon.
    notifier: Notifier,
    /// The consent book, shared with the D-Bus service: an answer never waits for the core.
    consents: Consents,
    /// The snapshot the tray icon reads (see [`tray::TrayIcon`]).
    view: Arc<ArcSwap<tray::View>>,
    /// Counts the pulses: the tray task makes the watcher re-read the snapshot on every one.
    refresh: watch::Sender<u64>,
    /// The merged events of the running engine (see `forward_transfers`).
    transfers: mpsc::UnboundedReceiver<TransferEvent>,
    /// The presses and other events of our notifications.
    notices: mpsc::UnboundedReceiver<Event>,
    /// The tray menu clicks and activation.
    actions: mpsc::UnboundedReceiver<tray::Action>,
    /// A sender of `actions` kept here: a tray that cannot start (no watcher on the bus) drops
    /// its own, and the channel must stay open, or `run` would end with it.
    _actions_tx: mpsc::UnboundedSender<tray::Action>,
    /// On a quit without work to lose, the process ends through this.
    quit: mpsc::UnboundedSender<()>,
    /// What one open notification answers, per server id.
    prompts: HashMap<u32, Button>,
    /// What one transfer needed: its event, the prompt of its consent, and its progress.
    tracked: HashMap<String, Tracked>,
    /// The view sent to the tray last: a different one makes the icon refresh itself.
    last: Option<tray::View>,
    /// The minutes left of the temporary visibility, as refreshed last: a step makes the
    /// tooltip say the new count.
    last_minutes: u64,
}

/// Starts the desktop legs and answers with the receiver of the quits of the user (a value is a
/// request; the end of the channel is not one, see `main`). Every leg runs as a task of its own,
/// and the big `select` of [`Desktop::run`] watches them.
pub fn spawn(
    core: Arc<Mutex<Core>>,
    connection: Connection,
    consents: Consents,
    notifier: Notifier,
    transfers: mpsc::UnboundedReceiver<TransferEvent>,
) -> mpsc::UnboundedReceiver<()> {
    let (notices, notices_rx) = mpsc::unbounded_channel();
    let (actions, actions_rx) = mpsc::unbounded_channel();
    let (quit, quit_rx) = mpsc::unbounded_channel();
    let (refresh, refresh_rx) = watch::channel(0);
    let view = Arc::new(ArcSwap::from_pointee(tray::View::default()));
    let pump = notifier.clone();
    tokio::spawn(pump.pump(notices));
    tokio::spawn(dnd_watch(core.clone(), connection.clone()));
    tokio::spawn(shell_watch(core.clone(), connection.clone()));
    tokio::spawn(run_tray(view.clone(), actions.clone(), refresh_rx));
    let desktop = Desktop {
        core,
        connection,
        notifier,
        consents,
        view,
        refresh,
        transfers,
        notices: notices_rx,
        actions: actions_rx,
        _actions_tx: actions,
        quit,
        prompts: HashMap::new(),
        tracked: HashMap::new(),
        last: None,
        last_minutes: 0,
    };
    tokio::spawn(desktop.run());
    quit_rx
}

/// The tray icon task: registers the item with the watcher, then makes the watcher re-read the
/// snapshot (the icon takes the whole state at once) on every pulse.
async fn run_tray(
    view: Arc<ArcSwap<tray::View>>,
    actions: mpsc::UnboundedSender<tray::Action>,
    mut refresh: watch::Receiver<u64>,
) {
    // In a sandbox the item may not take the well-known name `StatusNotifierItem-PID-ID`, the
    // way KStatusNotifierItem does (it registers by its connection name there).
    let sandboxed = Path::new(FLATPAK_INFO).exists();
    let icon = tray::TrayIcon::new(view, actions);
    let handle = match icon
        .disable_dbus_name(sandboxed)
        .assume_sni_available(true)
        .spawn()
        .await
    {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!("no tray icon: {e:#}");
            return;
        }
    };
    while let Ok(()) = refresh.changed().await {
        handle.update(|_| {}).await;
    }
}

impl Desktop {
    /// The state machine: one big `select` over the legs. It ends only when one of the channels
    /// does, which means the process is going out anyway (the tray's own sender is not one of
    /// them: see `_actions_tx`).
    async fn run(mut self) {
        self.pulse().await;
        let mut tick = tokio::time::interval(PULSE);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                transfer = self.transfers.recv() => match transfer {
                    Some(transfer) => self.on_transfer(transfer).await,
                    None => {
                        tracing::error!("the transfer events stopped: the desktop closes down");
                        return;
                    }
                },
                notice = self.notices.recv() => match notice {
                    Some(notice) => self.on_notice(notice).await,
                    None => {
                        tracing::error!("the notification events stopped: the desktop closes down");
                        return;
                    }
                },
                action = self.actions.recv() => match action {
                    Some(action) => {
                        self.on_action(action).await;
                    }
                    None => {
                        tracing::error!("the tray actions stopped: the desktop closes down");
                        return;
                    }
                },
                _ = tick.tick() => {
                    self.pulse().await;
                    self.re_show_missing().await;
                    self.forget_finished();
                }
            }
        }
    }

    // Transfers ------------------------------------------------------------------

    /// What one merged event of a transfer does: the prompt of its consent, the notices of its
    /// end, and the tray state.
    async fn on_transfer(&mut self, event: TransferEvent) {
        let id = event.id.clone();
        if !file_event(&mut self.tracked, &event) {
            return;
        }
        self.step_consent(&event).await;
        if is_transferring(&event.state) {
            let tracked = self.tracked.get_mut(&id).expect("tracked above");
            tracked.started = true;
        }
        self.pulse().await;
        if event.is_final() {
            self.finish_transfer(event).await;
        }
    }

    /// Keeps the prompt of `event` in the right shape while the transfer waits for the user,
    /// and drops it the moment the transfer does not wait any more.
    async fn step_consent(&mut self, event: &TransferEvent) {
        let id = event.id.clone();
        let step = {
            let tracked = self.tracked.get_mut(&id).expect("tracked above");
            if waits_consent(event) {
                match tracked.prompt {
                    Some(old) if tracked.token != event.consent => {
                        // The prompt of an older consent (the id came back) answers nothing now.
                        tracked.prompt = None;
                        tracked.dismissed = false;
                        PromptStep::Replace(old)
                    }
                    Some(_) => PromptStep::Keep,
                    None if tracked.dismissed => PromptStep::Dismissed,
                    None => PromptStep::Show,
                }
            } else {
                match tracked.prompt.take() {
                    Some(old) => PromptStep::Leave(old),
                    None => PromptStep::Keep,
                }
            }
        };
        match step {
            PromptStep::Keep | PromptStep::Dismissed => {}
            PromptStep::Show => self.show_consent(event, 0).await,
            PromptStep::Replace(old) => {
                self.prompts.remove(&old);
                self.notifier.close(old).await;
                self.show_consent(event, old).await;
            }
            PromptStep::Leave(old) => {
                // The transfer moved on: the prompt cannot answer anything now.
                self.prompts.remove(&old);
                self.notifier.close(old).await;
            }
        }
    }

    /// Shows (or replaces in place) the prompt of one consent, and keeps the answers reachable:
    /// the button map and the transfer's prompt pointer. Nothing is shown when the core does not
    /// wait for this consent any more (the event was stale), or when the daemon cannot be asked
    /// (the tick and the repeat events retry).
    async fn show_consent(&mut self, event: &TransferEvent, replaces: u32) {
        let id = event.id.clone();
        let Some(token) = self.consents.token(&id) else {
            tracing::debug!("no consent left to show for {id}");
            return;
        };
        {
            let tracked = self.tracked.get_mut(&id).expect("tracked above");
            tracked.prompt = None;
            tracked.token = token;
        }
        let offers_trust = {
            let core = self.core.lock().await;
            core.settings().trusted_devices
        } && !self
            .tracked
            .get(&id)
            .is_some_and(|tracked| tracked.no_trust)
            && id.contains(':');
        let notice = Notice::Consent {
            device: event.device.clone(),
            pin: event.pin.clone(),
            offer_trust: offers_trust,
        };
        let Some(server) = self.notifier.show(&notice, replaces).await else {
            // The tick looks for missing prompts: this consent is retried in 2 s.
            return;
        };
        let tracked = self.tracked.get_mut(&id).expect("tracked above");
        tracked.prompt = Some(server);
        tracked.dismissed = false;
        self.prompts.insert(server, Button::Consent { id, token });
    }

    /// Drops the state of the transfers that ended a while ago (see [`FINISHED_KEEP`]): the core
    /// outlives the window, so the map must not grow with every transfer of its life.
    fn forget_finished(&mut self) {
        self.tracked
            .retain(|_, t| t.finished.is_none_or(|at| at.elapsed() < FINISHED_KEEP));
    }

    /// Prompts again for every consent that waits for its user and shows none: after a restart
    /// of the daemon, or a daemon that came late. The tick does the same every 2 s, so a daemon
    /// starting late still gets its prompts.
    async fn re_show_missing(&mut self) {
        let due: Vec<TransferEvent> = self
            .tracked
            .values()
            .filter(|t| t.prompt.is_none() && !t.dismissed && waits_consent(&t.event))
            .map(|t| t.event.clone())
            .collect();
        for event in &due {
            self.show_consent(event, 0).await;
        }
    }

    /// The transfer is over: tells how it ended (see `appraise_final`) and marks it ended.
    async fn finish_transfer(&mut self, event: TransferEvent) {
        let id = event.id.clone();
        let (window_open, folder) = {
            let core = self.core.lock().await;
            (core.window_open(), download_dir(&core))
        };
        let tracked = self.tracked.get_mut(&id).expect("tracked above");
        let Some(notice) = conclude(tracked, &event, window_open) else {
            return;
        };
        if let Some(server) = self.notifier.show(&notice, 0).await {
            // The actions of the note must answer: what a press does, per server id.
            let button = match notice {
                Notice::Text { text, .. } => Some(Button::Text {
                    id: id.clone(),
                    text,
                }),
                Notice::Files { path, .. } => path.map_or_else(
                    || {
                        Some(Button::Folder {
                            dir: folder.clone(),
                        })
                    },
                    |path| {
                        Some(Button::File {
                            dir: folder.clone(),
                            path,
                        })
                    },
                ),
                Notice::Refusal { .. }
                | Notice::NotAccepted { .. }
                | Notice::Interrupted { .. }
                | Notice::Consent { .. } => None,
            };
            if let Some(button) = button {
                self.prompts.insert(server, button);
            }
        }
    }

    // Notifications --------------------------------------------------------------

    async fn on_notice(&mut self, event: Event) {
        match event {
            Event::Answered { server_id, key } => self.answer(server_id, &key).await,
            Event::Dismissed { server_id } => self.dismiss(server_id).await,
            Event::DaemonChanged => self.daemon_changed().await,
        }
        self.pulse().await;
    }

    /// A button of one of our notifications was pressed. The entry of the notice goes first, so
    /// a re-shown prompt of the same id (a refused trust) is kept whatever the server answers.
    async fn answer(&mut self, server_id: u32, key: &str) {
        let Some(button) = self.prompts.remove(&server_id) else {
            return; // not ours (the signals name every press of every app)
        };
        if !self.answer_button(&button, key).await {
            // Not one of its buttons: the prompt stays answerable, whatever closed it.
            self.prompts.insert(server_id, button);
        }
    }

    /// Applies one press. `false` when the key is none of the notice's actions.
    async fn answer_button(&mut self, button: &Button, key: &str) -> bool {
        match button {
            Button::Consent { id, token } => match key {
                ACCEPT => self.answer_consent(id, Some(*token), Answer::Accept).await,
                ACCEPT_AND_TRUST => {
                    self.answer_consent(id, Some(*token), Answer::AcceptAndTrust)
                        .await
                }
                DECLINE => self.answer_consent(id, Some(*token), Answer::Decline).await,
                _ => false,
            },
            Button::Text { id, text } => match key {
                COPY => {
                    self.set_clipboard(text).await;
                    true
                }
                OPEN_LINK => {
                    // Only a web link opens, whatever the text says.
                    let link = text.trim();
                    if notify::looks_like_link(link) {
                        self.open_anything(link).await;
                    } else {
                        tracing::warn!("not opening a text that is no http(s) link");
                    }
                    true
                }
                VIEW => {
                    self.spawn_window(["--transfer", id]).await;
                    true
                }
                _ => false,
            },
            Button::File { dir, path } => match key {
                OPEN_FILE => {
                    self.open_anything(&file_uri(path)).await;
                    true
                }
                OPEN_FOLDER => {
                    self.open_anything(&file_uri(dir)).await;
                    true
                }
                _ => false,
            },
            Button::Folder { dir } => match key {
                OPEN_FOLDER => {
                    self.open_anything(&file_uri(dir)).await;
                    true
                }
                _ => false,
            },
        }
    }

    /// One answer of a consent prompt, as `AnswerTransfer` takes it on the bus. A trust that
    /// cannot be applied shows the prompt again without the choice: the transfer still waits.
    /// Nothing waits any more (a late click, an already answered consent, a stopped service):
    /// the prompt is over with no answer.
    async fn answer_consent(&mut self, id: &str, token: Option<u64>, answer: Answer) -> bool {
        match self
            .consents
            .resolve(id, token, answer, By::User, Instant::now())
        {
            Ok(resolved) => {
                if answer == Answer::AcceptAndTrust
                    && let Some(mac) = resolved.mac
                {
                    self.core.lock().await.remember_device(resolved.device, mac);
                }
                true
            }
            Err(
                error @ (ConsentError::NoMac | ConsentError::NoName | ConsentError::DeviceChanged),
            ) => {
                tracing::info!("the transfer still waits after a refused answer: {error}");
                self.ask_again_without_trust(id).await;
                true
            }
            Err(error) => {
                tracing::debug!("the prompt of {id} answered too late or no waits exists: {error}");
                true
            }
        }
    }

    /// The core could not apply the trust of "Accept and Trust": the prompt shows again without
    /// the choice that failed.
    async fn ask_again_without_trust(&mut self, id: &str) {
        let Some(tracked) = self.tracked.get_mut(id) else {
            return;
        };
        tracked.no_trust = true;
        tracked.dismissed = false;
        let event = tracked.event.clone();
        let old = tracked.prompt.take();
        if let Some(old) = old {
            self.prompts.remove(&old);
        }
        self.show_consent(&event, old.unwrap_or(0)).await;
    }

    /// A notice went away without a press. A consent prompt that this was gets its pointer
    /// cleared: the transfer still waits, so the user can answer it in the window.
    async fn dismiss(&mut self, server_id: u32) {
        let Some(Button::Consent { id, .. }) = self.prompts.remove(&server_id) else {
            return;
        };
        let Some(tracked) = self.tracked.get_mut(&id) else {
            return;
        };
        if tracked.prompt == Some(server_id) {
            tracked.prompt = None;
            tracked.dismissed = true;
        }
    }

    /// The daemon changed (appeared, went, or was replaced): whatever it showed is gone, and
    /// every waiting consent is prompted again at once.
    async fn daemon_changed(&mut self) {
        self.prompts.clear();
        for tracked in self.tracked.values_mut() {
            tracked.prompt = None;
            tracked.dismissed = false;
        }
        self.re_show_missing().await;
    }

    // Tray actions ----------------------------------------------------------------

    async fn on_action(&mut self, action: tray::Action) {
        match action {
            tray::Action::OpenWindow(page) => match page {
                Some("settings") => self.spawn_window(["--page", "settings"]).await,
                _ => self.spawn_window([]).await,
            },
            tray::Action::OpenFiles => self.spawn_window(["--open-files"]).await,
            tray::Action::PasteClipboard => self.spawn_window(["--clipboard"]).await,
            tray::Action::SetVisible => {
                {
                    let mut core = self.core.lock().await;
                    let next = !core.settings().visible;
                    core.set_visible(next);
                }
                crate::dbus::emit_visibility_changed(&self.connection).await;
            }
            tray::Action::ToggleTemporaryVisible => {
                // The label stays the same in both states: a click starts the visibility, or
                // takes it back while it runs.
                {
                    let mut core = self.core.lock().await;
                    let running = core.temp_visible_active();
                    core.make_visible_for(if running { 0 } else { TEMP_VISIBLE_SECS });
                }
                crate::dbus::emit_visibility_changed(&self.connection).await;
            }
            tray::Action::Quit => {
                let (in_flight, window_open) = {
                    let core = self.core.lock().await;
                    (core.transfer_in_flight(), core.window_open())
                };
                if in_flight || window_open {
                    // A transfer would be lost, or the window shows and must close too: the
                    // shell asks before it stops the service (`--request-quit` shows its
                    // "Quit KuickShare?" prompt when a transfer is in flight).
                    self.spawn_window(["--request-quit"]).await;
                } else {
                    // Idle: the background service itself is the whole app, and it may stop.
                    let _ = self.quit.send(());
                }
            }
        }
        self.pulse().await;
    }

    // Tools -----------------------------------------------------------------------

    /// Sends the snapshot the tray reads, and makes the watcher re-read it when something
    /// changed or the minutes of the temporary visibility went over to the next count.
    async fn pulse(&mut self) {
        let view = {
            let core = self.core.lock().await;
            tray::View {
                enabled: core.settings().tray_icon,
                visible: core.effective_visible(),
                visible_until: core.visible_until(),
                waiting: core.transfer_waiting(),
                busy: core.transfer_busy(),
            }
        };
        let now = history::now();
        let changed = self.last.as_ref() != Some(&view);
        let minutes = minutes_left(view.visible_until, now);
        let countdown = view.visible_until > now && self.last_minutes != minutes;
        if !changed && !countdown {
            return;
        }
        self.last_minutes = minutes;
        self.view.store(Arc::new(view.clone()));
        self.refresh.send_modify(|pulses| *pulses += 1);
        self.last = Some(view);
    }

    /// Starts the shell for the user to act on. The core never waits for it.
    async fn spawn_window(&self, args: impl IntoIterator<Item = &str>) {
        let args: Vec<&str> = args.into_iter().collect();
        let program = match shell_program() {
            Some(program) => program,
            None => {
                tracing::error!("cannot find the shell to open the window");
                return;
            }
        };
        if Path::new(FLATPAK_INFO).exists() {
            // The portal takes the path and the arguments as NUL-terminated byte strings.
            let mut argv: Vec<Vec<u8>> = Vec::with_capacity(args.len() + 1);
            let mut bytes = program.as_os_str().as_encoded_bytes().to_vec();
            bytes.push(0);
            argv.push(bytes);
            for arg in args.iter() {
                let mut bytes = arg.as_bytes().to_vec();
                bytes.push(0);
                argv.push(bytes);
            }
            if self.spawn_through_portal(argv).await {
                return;
            }
            // The portal refused (rare): the direct start below is the fallback it gets, and
            // the shell binary is inside the same sandbox either way.
        }
        let mut command = process::Command::new(&program);
        if app_image().is_some() {
            // The variables of the running image point at its own mount (see `Launcher`); the
            // new one sets them for itself.
            command.env_remove("ARGV0").env_remove("APPDIR");
        }
        if let Err(e) = command.args(args).spawn() {
            tracing::warn!("cannot open the window: {e}");
        }
    }

    /// The Flatpak portal `Spawn` call: the shell starts inside the same sandbox as this core,
    /// with everything seen from the same paths as this process sees. `false` when it failed.
    async fn spawn_through_portal(&self, argv: Vec<Vec<u8>>) -> bool {
        // No file descriptors to hand over, as an empty `a{uh}` (in the portalled `Spawn` the
        // map's value type decides, and an empty generic map would arrive as `a{sv}`). Also no
        // extra environment, and the version flag of the newest portal interface.
        let body = (
            b"/\0".to_vec(), // cwd at the root of the sandbox, a NUL-terminated byte string (`ay`)
            argv,
            HashMap::<u32, Fd>::new(),
            HashMap::<String, String>::new(),
            2u32, // LATEST_VERSION
            HashMap::<String, Variant<'static>>::new(),
        );
        match self
            .connection
            .call_method(
                Some("org.freedesktop.portal.Flatpak"),
                PORTAL_PATH,
                Some("org.freedesktop.portal.Flatpak"),
                "Spawn",
                &body,
            )
            .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("the flatpak portal could not start the window: {e}");
                false
            }
        }
    }

    /// Puts text on the clipboard with what this task has: klipper on the bus first, then
    /// `wl-copy`, then `xclip`.
    async fn set_clipboard(&self, text: &str) {
        let klipper = self
            .connection
            .call_method(
                Some("org.kde.klipper.klipper"),
                "/klipper",
                Some("org.kde.klipper.klipper"),
                "setClipboardContents",
                &(&text,),
            )
            .await;
        match klipper {
            Ok(_) => return,
            // A Plasma runs klipper; elsewhere one of the command line tools is next.
            Err(e) => tracing::debug!("no klipper on the bus for the clipboard: {e}"),
        }
        for (program, arguments) in [
            ("wl-copy", vec![]),
            ("xclip", vec!["-selection", "clipboard", "-in"]),
        ] {
            let mut child = match process::Command::new(program)
                .args(arguments)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    tracing::debug!("no {program} for the clipboard: {e}");
                    continue;
                }
            };
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(e) = stdin.write_all(text.as_bytes()).await {
                    tracing::debug!("cannot write to {program}: {e}");
                }
                drop(stdin);
            }
            // The process stays up to serve the pastes until something replaces it.
            return;
        }
        tracing::warn!("nothing can set the clipboard");
    }

    /// Opens a thing (a link, a file or a folder): the OpenURI portal in a sandbox, `xdg-open`
    /// everywhere else.
    async fn open_anything(&self, what: &str) {
        if Path::new(FLATPAK_INFO).exists() {
            let body = (
                "", // no parent window to come back from
                what,
                HashMap::<String, Variant<'static>>::new(),
            );
            if let Err(e) = self
                .connection
                .call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some("org.freedesktop.portal.OpenURI"),
                    "OpenURI",
                    &body,
                )
                .await
            {
                tracing::warn!("the flatpak portal could not open {what}: {e}");
            }
            return;
        }
        if let Err(e) = process::Command::new("xdg-open").arg(what).spawn() {
            tracing::warn!("cannot open {what}: {e}");
        }
    }
}

// The end of a transfer ---------------------------------------------------------------

/// Files `event` under its transfer id. `false` when it only repeats an end already told (a
/// `Disconnected` after a `Finished`): nothing new to do. A live event under an ended id means
/// the id (`ble:XXXX`, `ip:port`) came back for another transfer, which starts clean.
fn file_event(tracked: &mut HashMap<String, Tracked>, event: &TransferEvent) -> bool {
    let entry = tracked.entry(event.id.clone()).or_default();
    if entry.finished.is_some() {
        if event.is_final() {
            return false;
        }
        *entry = Tracked::default();
    }
    entry.event = event.clone();
    true
}

/// Marks the transfer ended and says how, if it is worth a notice (see `appraise_final`). Marked
/// whatever the verdict: a quiet end is an end too, and the `Disconnected` that follows it must
/// not tell a story of its own.
fn conclude(tracked: &mut Tracked, event: &TransferEvent, window_open: bool) -> Option<Notice> {
    tracked.finished = Some(Instant::now());
    appraise_final(event, tracked, window_open)
}

/// The notice for one final event, or `None` for a quiet end. An end in front of an open window
/// stays silent (its toast says it there); the refusal of an offer and the refusal of an
/// unaccepted send do not count: those want to be seen always.
fn appraise_final(event: &TransferEvent, tracked: &Tracked, window_open: bool) -> Option<Notice> {
    let inbound = event.kind == "inbound";
    let refusal = refusal_text(&event.reason);
    if inbound && !refusal.is_empty() {
        // The library refused the offer itself (too many files, not enough space, ...): always
        // shown, the in-app toast does not give the reason.
        return Some(Notice::Refusal {
            device: event.device.clone(),
            reason: refusal,
        });
    }
    if !inbound && !tracked.started && (event.state == "Disconnected" || event.state == "Rejected")
    {
        // The library reports a refusal by the other device as "Disconnected"; before any data
        // moved that is the most likely reason (it can also be a full disk or a timeout). Always
        // shown: the in-app toast does not say what to check.
        return Some(Notice::NotAccepted {
            device: event.device.clone(),
            over_bluetooth: is_ble(&event.id),
        });
    }
    if window_open {
        return None;
    }
    if event.state == "Finished" && inbound {
        return Some(received_notice(event));
    }
    if event.state == "Disconnected" {
        return Some(Notice::Interrupted {
            device: event.device.clone(),
        });
    }
    None
}

/// What a finished inbound transfer says: for text and links the copy-open-view actions, for
/// files the open ones.
fn received_notice(event: &TransferEvent) -> Notice {
    if event.is_textual() {
        let text = if event.text.is_empty() {
            event.preview.clone()
        } else {
            event.text.clone()
        };
        // One http(s) link, whatever the payload kind says: the sender's "Url" may be any scheme.
        let is_link = notify::looks_like_link(text.trim());
        Notice::Text {
            device: event.device.clone(),
            text,
            is_link,
        }
    } else {
        // Several files: only the folder makes sense.
        let saved: Vec<String> = event
            .saved_paths
            .iter()
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();
        let path = if saved.len() == 1 {
            Some(saved.first().expect("exactly one").clone())
        } else {
            None
        };
        Notice::Files {
            device: event.device.clone(),
            path,
        }
    }
}

// Do Not Disturb --------------------------------------------------------------------

/// The notification daemon's Do Not Disturb mode: while it is on and `HideWhenDnd` is set, this
/// computer hides (see `Core::set_dnd`). `Inhibited` is read when the daemon (re)appears and
/// whenever it changes; the option is applied by the core itself when it changes.
async fn dnd_watch(core: Arc<Mutex<Core>>, connection: Connection) {
    let service = "org.freedesktop.Notifications";
    loop {
        let properties = match fdo::PropertiesProxy::builder(&connection)
            .destination(service)
            .path("/org/freedesktop/Notifications")
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
        {
            Ok(properties) => properties,
            Err(e) => {
                tracing::debug!("no notification daemon to watch Do Not Disturb: {e}");
                tokio::time::sleep(notify::RETRY_DELAY).await;
                continue;
            }
        };
        let mut inhibited = read_inhibited(&properties, service).await;
        let (Ok(mut changes), Ok(mut owners)) = (
            properties.receive_properties_changed().await,
            owner_changes(&connection).await,
        ) else {
            tracing::debug!("no Do Not Disturb watch on the notification daemon");
            tokio::time::sleep(notify::RETRY_DELAY).await;
            continue;
        };
        // What the core was last told: an event of the bus that changes nothing says nothing.
        let mut applied: Option<bool> = None;
        loop {
            if let Some(on) = inhibited
                && applied != inhibited
            {
                core.lock().await.set_dnd(on);
                applied = inhibited;
                // `Suspended` may have changed, and an open window shows it.
                crate::dbus::emit_visibility_changed(&connection).await;
            }
            tokio::select! {
                change = changes.next() => {
                    let Some(change) = change else {
                        break;
                    };
                    let Ok(args) = change.args() else {
                        break;
                    };
                    if args.interface_name.as_str() != service {
                        continue;
                    }
                    if let Some(value) = args.changed_properties.get("Inhibited") {
                        inhibited = bool::try_from(value).ok();
                    } else if args.invalidated_properties.contains(&"Inhibited") {
                        // The daemon only said the value is not valid any more: read it again.
                        inhibited = read_inhibited(&properties, service).await;
                    }
                }
                owner = owners.next() => {
                    let Some(owner) = owner else {
                        break;
                    };
                    if let Ok(args) = owner.args()
                        && args.name.as_str() == service
                    {
                        // The daemon came, went or was replaced: it keeps no state of the old
                        // one. One that is gone, or does not serve `Inhibited`, is no Do Not
                        // Disturb.
                        inhibited = Some(if args.new_owner.is_some() {
                            read_inhibited(&properties, service).await.unwrap_or(false)
                        } else {
                            false
                        });
                    }
                }
            }
        }
        // The daemon (or its properties) is gone for now: rebuilt in a pause.
        tokio::time::sleep(notify::RETRY_DELAY).await;
    }
}

/// The `Inhibited` of the daemon, as it answers `Get`.
async fn read_inhibited(properties: &fdo::PropertiesProxy<'static>, service: &str) -> Option<bool> {
    let interface: zbus::names::InterfaceName<'_> = service.try_into().ok()?;
    let value = properties.get(interface, "Inhibited").await.ok()?;
    bool::try_from(&value).ok()
}

/// The bus watch for `NameOwnerChanged`.
async fn owner_changes(connection: &Connection) -> zbus::Result<fdo::NameOwnerChangedStream> {
    fdo::DBusProxy::builder(connection)
        .build()
        .await?
        .receive_name_owner_changed()
        .await
}

/// Takes `WindowOpen` back when the shell leaves the bus with its window open (a crash, a kill):
/// nothing else would ever clear it, and the end-of-transfer notifications would stay off.
async fn shell_watch(core: Arc<Mutex<Core>>, connection: Connection) {
    let mut owners = match owner_changes(&connection).await {
        Ok(owners) => owners,
        Err(e) => {
            tracing::debug!("no watch on the shell's bus name: {e}");
            return;
        }
    };
    while let Some(owner) = owners.next().await {
        if let Ok(args) = owner.args()
            && args.name.as_str() == SHELL_NAME
            && args.old_owner.is_some()
        {
            core.lock().await.set_window_open(false);
        }
    }
}

// Tests ------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::i18n;

    /// A text transfer that has finished.
    fn received(id: &str, text: &str) -> TransferEvent {
        let mut event = TransferEvent::for_test(id, "inbound", "Finished", "Pixel");
        event.payload_kind = "Text".to_owned();
        event.text = text.to_owned();
        event
    }

    #[test]
    fn refusals_come_out_in_the_same_words() {
        assert_eq!(
            refusal_text("too_many_files"),
            i18n!("The offer has too many files")
        );
        assert_eq!(
            refusal_text("not_enough_space"),
            i18n!("There is not enough free space in the download folder")
        );
        assert_eq!(
            refusal_text("destination"),
            i18n!("The download folder cannot be used")
        );
        assert_eq!(
            refusal_text("bad_name"),
            i18n!("A file name in the offer cannot be saved")
        );
        assert_eq!(refusal_text("bad_size"), i18n!("The offer is malformed"));
        assert_eq!(
            refusal_text("duplicate_id"),
            i18n!("The offer is malformed")
        );
        assert_eq!(refusal_text(""), "");
        assert_eq!(
            refusal_text("some_future_code"),
            i18n!("The offer was refused")
        );
    }

    #[test]
    fn the_offer_of_a_sender_the_library_refused() {
        let mut event = TransferEvent::for_test("10.0.0.5:1", "inbound", "Rejected", "Pixel");
        event.reason = "too_many_files".to_owned();
        assert!(matches!(
            appraise_final(&event, &Tracked::default(), false),
            Some(Notice::Refusal { device, .. }) if device == "Pixel"
        ));
    }

    #[test]
    fn an_unaccepted_send_says_bluetooth_when_it_came_over_one() {
        let event = TransferEvent::for_test("ble:1234", "outbound", "Disconnected", "Phone");
        let tracked = Tracked::default();
        assert!(matches!(
            appraise_final(&event, &tracked, false),
            Some(Notice::NotAccepted {
                over_bluetooth: true,
                ..
            })
        ));
    }

    #[test]
    fn the_open_window_swallows_the_ends_but_not_the_refusals() {
        let refused = TransferEvent::for_test("10.0.0.5:1", "outbound", "Rejected", "Pixel");
        assert!(appraise_final(&refused, &Tracked::default(), true).is_some());
        assert_eq!(
            appraise_final(&received("10.0.0.5:1", "hello"), &Tracked::default(), true),
            None
        );
        assert!(
            appraise_final(&received("10.0.0.5:1", "hello"), &Tracked::default(), false).is_some()
        );
    }

    #[test]
    fn received_text_tells_its_links() {
        let link = received("10.0.0.5:1", "https://example.org/path");
        assert!(matches!(
            appraise_final(&link, &Tracked::default(), false),
            Some(Notice::Text { is_link: true, .. })
        ));
        let text = received("10.0.0.5:1", "just words");
        assert!(matches!(
            appraise_final(&text, &Tracked::default(), false),
            Some(Notice::Text { is_link: false, .. })
        ));
    }

    #[test]
    fn received_files_offer_whats_there() {
        let mut event = TransferEvent::for_test("10.0.0.5:1", "inbound", "Finished", "Pixel");
        event.saved_paths = vec!["/home/u/Downloads/photo.jpg".to_owned()];
        match appraise_final(&event, &Tracked::default(), false) {
            Some(Notice::Files { path, .. }) => {
                assert_eq!(path.as_deref(), Some("/home/u/Downloads/photo.jpg"));
            }
            other => panic!("not a files notice: {other:?}"),
        }
        // Two files: only the folder makes sense.
        event.saved_paths = vec![
            "/home/u/Downloads/photo.jpg".to_owned(),
            "/home/u/Downloads/photo2.jpg".to_owned(),
        ];
        match appraise_final(&event, &Tracked::default(), false) {
            Some(Notice::Files { path, .. }) => assert_eq!(path, None),
            other => panic!("not a files notice: {other:?}"),
        }
    }

    #[test]
    fn a_lost_connection_says_it() {
        let event = TransferEvent::for_test("10.0.0.5:1", "inbound", "Disconnected", "Pixel");
        assert!(matches!(
            appraise_final(&event, &Tracked::default(), false),
            Some(Notice::Interrupted { .. })
        ));
    }

    #[test]
    fn quiet_ends() {
        // A cancellation is the user's own doing.
        let cancelled = TransferEvent::for_test("10.0.0.5:1", "inbound", "Cancelled", "Pixel");
        assert_eq!(appraise_final(&cancelled, &Tracked::default(), false), None);
        // A started send whose peer refused it already had its Active tray state.
        let rejected = TransferEvent::for_test("10.0.0.5:1", "outbound", "Rejected", "Pixel");
        let tracked = Tracked {
            started: true,
            ..Tracked::default()
        };
        assert_eq!(appraise_final(&rejected, &tracked, false), None);
    }

    /// Runs the events of one transfer through the bookkeeping `on_transfer` does, and collects
    /// the notices of the ends.
    fn notices(events: &[TransferEvent], window_open: bool) -> Vec<Option<Notice>> {
        let mut tracked = HashMap::new();
        let mut out = Vec::new();
        for event in events {
            if !file_event(&mut tracked, event) {
                continue;
            }
            if is_transferring(&event.state) {
                tracked.get_mut(&event.id).expect("filed above").started = true;
            }
            if event.is_final() {
                let entry = tracked.get_mut(&event.id).expect("filed above");
                out.push(conclude(entry, event, window_open));
            }
        }
        out
    }

    #[test]
    fn a_disconnect_after_the_quiet_end_of_a_send_says_nothing() {
        for window_open in [false, true] {
            let events = [
                TransferEvent::for_test("ble:1234", "outbound", "SendingFiles", "Phone"),
                TransferEvent::for_test("ble:1234", "outbound", "Finished", "Phone"),
                TransferEvent::for_test("ble:1234", "outbound", "Disconnected", "Phone"),
            ];
            // One verdict only, the quiet one: no "connection lost" for a send that finished.
            assert_eq!(notices(&events, window_open), [None]);
        }
        // Without any data moved, the same order must not become "not accepted" either.
        let events = [
            TransferEvent::for_test("ble:1234", "outbound", "Finished", "Phone"),
            TransferEvent::for_test("ble:1234", "outbound", "Disconnected", "Phone"),
        ];
        assert_eq!(notices(&events, false), [None]);
    }

    #[test]
    fn an_id_that_comes_back_starts_clean() {
        let events = [
            TransferEvent::for_test("ble:1234", "outbound", "SendingFiles", "Phone"),
            TransferEvent::for_test("ble:1234", "outbound", "Finished", "Phone"),
            // The second send to the same phone: nothing moved before it was refused.
            TransferEvent::for_test("ble:1234", "outbound", "Initial", "Phone"),
            TransferEvent::for_test("ble:1234", "outbound", "Disconnected", "Phone"),
        ];
        let out = notices(&events, false);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], None);
        assert!(matches!(
            out[1],
            Some(Notice::NotAccepted {
                over_bluetooth: true,
                ..
            })
        ));
    }

    #[test]
    fn only_a_http_link_is_a_link() {
        // The sender's "Url" says nothing about the scheme.
        let mut file = received("10.0.0.5:1", "file:///etc/passwd");
        file.payload_kind = "Url".to_owned();
        assert!(matches!(
            appraise_final(&file, &Tracked::default(), false),
            Some(Notice::Text { is_link: false, .. })
        ));
        let mut web = received("10.0.0.5:1", "  https://example.org/a  ");
        web.payload_kind = "Url".to_owned();
        assert!(matches!(
            appraise_final(&web, &Tracked::default(), false),
            Some(Notice::Text { is_link: true, .. })
        ));
    }

    #[test]
    fn the_minutes_of_the_tooltip() {
        let now = 3_600_000;
        assert_eq!(minutes_left(now + 30, now), 1);
        assert_eq!(minutes_left(now + 90, now), 2);
        assert_eq!(minutes_left(now + 660, now), 11);
        assert_eq!(minutes_left(now, now), 0);
        assert_eq!(minutes_left(now - 100, now), 0);
    }

    #[test]
    fn file_uris_keep_the_path_bytes() {
        assert_eq!(
            file_uri("/home/user/Downloads/a b (2).jpg"),
            "file:///home/user/Downloads/a%20b%20(2).jpg"
        );
        // Every byte stays what it was: a UTF-8 path travels as its bytes.
        assert_eq!(
            file_uri("/home/user/rom\u{f6}"),
            "file:///home/user/rom%C3%B6"
        );
        assert_eq!(file_uri("/"), "file:///");
    }
}
