//! Notifications on the freedesktop interface.
//!
//! The daemon is `org.freedesktop.Notifications` on the session bus.
//! Prompts for incoming transfers come back as `ActionInvoked`; without a daemon
//! nothing is shown and the transfers still wait for the core, like in the shell.
//!
//! What to show is decided in [`crate::desktop`]; only the protocol and the texts
//! live here.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use zbus::{Connection, Variant, fdo, proxy::CacheProperties};

use crate::i18n::{i18n, i18nc};

/// The words of one answer, as `AnswerTransfer` takes them.
pub const ACCEPT: &str = "accept";
pub const ACCEPT_AND_TRUST: &str = "accept_and_trust";
pub const DECLINE: &str = "decline";

/// The keys of the buttons of a text or link notice.
pub const COPY: &str = "copy";
pub const OPEN_LINK: &str = "open";
pub const VIEW: &str = "view";

/// The keys of the buttons of a files notice.
pub const OPEN_FILE: &str = "open_file";
pub const OPEN_FOLDER: &str = "open_folder";

/// The name the daemon offers, also for the owner watch of [`Notifier::pump`].
const SERVICE: &str = "org.freedesktop.Notifications";

/// While the daemon is gone, the notifications are retried this often.
pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(3);

/// The expiries the shell's notifications had.
const NOTICE_TIMEOUT: i32 = 7_000;
/// A prompt stays until it is answered, as the shell's persistent ones did.
const PERSISTENT: i32 = 0;

/// Icons: the app icon everywhere, and the warning one when a transfer failed.
const APP_ICON: &str = "io.github.the_beper.KuickShare";
const WARNING_ICON: &str = "dialog-warning";

const APP_NAME: &str = "KuickShare";

mod proxies {
    use std::collections::HashMap;

    use zbus::{Variant, proxy};

    #[proxy(
        interface = "org.freedesktop.Notifications",
        default_service = "org.freedesktop.Notifications",
        default_path = "/org/freedesktop/Notifications"
    )]
    pub trait Notifications {
        /// Shows a notification and answers with its id. `replaces_id` of 0 makes a new one.
        ///
        /// `actions` holds action key and label, in turns (the freedesktop order: key first),
        /// `expire_timeout` waits 0 seconds (the user closes it), or -1 for the server.
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            app_name: &str,
            replaces_id: u32,
            app_icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<&str>,
            hints: HashMap<&str, Variant<'_>>,
            expire_timeout: i32,
        ) -> zbus::Result<u32>;

        /// Closes one notification; fails for a foreign or unknown id.
        fn close_notification(&self, id: u32) -> zbus::Result<()>;

        /// A button of one of our notifications was pressed.
        #[zbus(signal)]
        fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

        /// The daemon closed one of our notifications: a click on the body, the timeout,
        /// or the banner cleared.
        #[zbus(signal)]
        fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
    }
}

/// What the desktop asks the notifier to display. One prompt and one result of a transfer id
/// may be up at once: the caller replaces them by their server ids, and keeps what the actions
/// do in its own button map (see [`crate::desktop`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Notice {
    /// A prompt that waits for an answer; it replaces a previous prompt of the same id.
    ///
    /// `offer_trust` offers "Accept and Trust"; after a failed attempt the prompt is
    /// shown again without the choice that cannot be applied (as the shell did).
    Consent {
        device: String,
        pin: String,
        offer_trust: bool,
    },
    /// Text or a link arrived; the actions copy it, open it, or open the transfer.
    Text {
        device: String,
        text: String,
        is_link: bool,
    },
    /// Files arrived into the download folder; the actions open the file or the folder.
    Files {
        device: String,
        /// The one file to open; `None` opens the folder only.
        path: Option<String>,
    },
    /// The offer was refused before anything was accepted.
    Refusal { device: String, reason: String },
    /// Nothing moved yet and the transfer is over before it could start.
    NotAccepted {
        device: String,
        over_bluetooth: bool,
    },
    /// The connection was lost, with data already on the wire.
    Interrupted { device: String },
}

/// What the notifier reports back to the desktop.
pub enum Event {
    /// A button of one of our notifications was pressed; `key` is one of the action
    /// keys below.
    Answered { server_id: u32, key: String },
    /// A notification is gone without an answer: a click on the body, the timeout, or
    /// a banner the server cleared.
    Dismissed { server_id: u32 },
    /// The daemon changed (appeared, went, or replaced): everything it showed is gone and
    /// new listeners are in place. Whatever the desktop showed again is its business.
    DaemonChanged,
}

/// One notification to show, as the flat `Notify` call takes it.
struct Text {
    icon: &'static str,
    summary: String,
    body: String,
    /// Action key and label, in turns (the freedesktop order: key first).
    actions: Vec<&'static str>,
    expire: i32,
}

/// What a notice says and does when it is shown.
fn text_of(notice: &Notice) -> Text {
    match notice {
        Notice::Consent {
            device,
            pin,
            offer_trust,
        } => {
            let who = plain_who(device, "A nearby device");
            let mut body = if pin.is_empty() {
                i18n!("%1 wants to share with you", &who)
            } else {
                let pin_text = plain(pin);
                i18n!("%1 wants to share with you\nPIN: %2", &who, &pin_text)
            };
            if *offer_trust {
                body.push('\n');
                body.push_str(&i18n!(
                    "\u{201C}Accept and Trust\u{201D} recognises this device by its name and network \
                     address, which another device on the same network can copy",
                ));
            }
            let mut actions = vec![button(ACCEPT, "Accept")];
            if *offer_trust {
                actions.push(button(ACCEPT_AND_TRUST, "Accept and Trust"));
            }
            actions.push(button(DECLINE, "Decline"));
            Text {
                icon: APP_ICON,
                summary: i18nc!("@title:notification", "Incoming Transfer"),
                body,
                actions: pairs(actions),
                expire: PERSISTENT,
            }
        }
        Notice::Text {
            device,
            text,
            is_link,
        } => {
            let who = plain_who(device, "a nearby device");
            let shortened = plain(&shortened(text));
            let mut actions = vec![if *is_link {
                button(COPY, "Copy Link")
            } else {
                button(COPY, "Copy Text")
            }];
            if *is_link {
                actions.push(button(OPEN_LINK, "Open Link"));
            }
            actions.push(button(VIEW, "View"));
            Text {
                icon: APP_ICON,
                summary: if *is_link {
                    i18nc!("@title:notification", "Link Received")
                } else {
                    i18nc!("@title:notification", "Text Received")
                },
                body: i18n!("From %1:\n%2", &who, &shortened),
                actions: pairs(actions),
                expire: NOTICE_TIMEOUT,
            }
        }
        Notice::Files { device, path } => {
            let who = plain_who(device, "a nearby device");
            let mut actions = Vec::new();
            if path.is_some() {
                actions.push(button(OPEN_FILE, "Open File"));
            }
            actions.push(button(OPEN_FOLDER, "Open Folder"));
            Text {
                icon: APP_ICON,
                summary: i18nc!("@title:notification", "Files Received"),
                body: i18n!("From %1", &who),
                actions: pairs(actions),
                expire: NOTICE_TIMEOUT,
            }
        }
        Notice::Refusal { device, reason } => Text {
            icon: WARNING_ICON,
            summary: i18nc!("@title:notification", "Offer Refused"),
            body: i18n!("From %1: %2", &plain_who(device, "a nearby device"), reason),
            actions: Vec::new(),
            expire: NOTICE_TIMEOUT,
        },
        Notice::NotAccepted {
            device,
            over_bluetooth,
        } => {
            let who = plain_who(device, "a nearby device");
            let body = if *over_bluetooth {
                i18n!(
                    "Could not start the transfer to %1 over Bluetooth\nCheck that Bluetooth is on and \
                     the device is nearby, then try again",
                    &who,
                )
            } else {
                i18n!(
                    "%1 declined the transfer or could not receive it\nCheck the other device and its \
                     free space, then try again",
                    &who,
                )
            };
            Text {
                icon: WARNING_ICON,
                summary: i18nc!("@title:notification", "Transfer Not Accepted"),
                body,
                actions: Vec::new(),
                expire: NOTICE_TIMEOUT,
            }
        }
        Notice::Interrupted { device } => Text {
            icon: WARNING_ICON,
            summary: i18nc!("@title:notification", "Transfer Interrupted"),
            body: i18n!(
                "The connection with %1 was lost\nKeep both devices close and awake, then try again",
                &plain_who(device, "a nearby device"),
            ),
            actions: Vec::new(),
            expire: NOTICE_TIMEOUT,
        },
    }
}

/// A button: the key of our action, and its label in one of the catalogs.
fn button(key: &'static str, label: &'static str) -> (&'static str, String) {
    (key, i18nc!("@action:button", label))
}

/// The flat action list of the daemon: the key and the label, in turns. The labels out
/// of [`crate::i18n`] must outlive the call (see [`to_static`]).
fn pairs(actions: Vec<(&'static str, String)>) -> Vec<&'static str> {
    actions
        .into_iter()
        .flat_map(|(key, label)| [key, to_static(&label)])
        .collect()
}

/// Keeps one string alive for `'static` (see [`Box::leak`]): a class of texts of a few
/// words per notification shown, freed when the daemon drops its copy.
fn to_static(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

/// A `<` or `>` cannot reach a body text: the names and the previews come from
/// senders, and the bodies may be read as markup. What `CoreBridge::plain` did with the
/// in-app texts, and what the shell's escaped texts ended up shown as.
fn plain(text: &str) -> String {
    text.replace('<', "\u{2039}").replace('>', "\u{203A}")
}

/// The device to name, or the translation of `fallback`: senders may omit the name, and
/// the shell said "a nearby device" then (capitalised in a prompt).
fn plain_who(device: &str, fallback: &'static str) -> String {
    plain(&if device.is_empty() {
        i18n!(fallback)
    } else {
        device.to_owned()
    })
}

/// The text shown at most 200 characters, as the shell did with previews.
fn shortened(text: &str) -> String {
    if text.len() > 200 {
        let cut = text.char_indices().nth(200).map_or(text.len(), |(i, _)| i);
        format!("{}\u{2026}", &text[..cut])
    } else {
        text.to_owned()
    }
}

/// Whether `text` is one link, as the shell decided: of the shape `http://…` or
/// `https://…` (any case) with no spaces (`^https?://\S+$` on the trimmed text).
pub fn looks_like_link(text: &str) -> bool {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    else {
        return false;
    };
    !rest.is_empty() && !text.chars().any(char::is_whitespace)
}

/// The client of the daemon; it never takes the `Core` lock.
#[derive(Clone)]
pub struct Notifier {
    connection: Connection,
}

impl Notifier {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    /// A proxy against whatever daemon serves the name now. Not cached: a show is a
    /// few calls on a quiet day, and the plasmashell restarting must not outdate it.
    async fn proxy(&self) -> zbus::Result<proxies::NotificationsProxy<'static>> {
        proxies::NotificationsProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    /// Shows one notice, replacing `replaces`'s previous one whenever it belongs to the
    /// same transfer, and answers with the server id.
    pub async fn show(&self, notice: &Notice, replaces: u32) -> Option<u32> {
        let text = text_of(notice);
        // The urgency is normal for everything the shell showed with `NormalUrgency`.
        let hints: HashMap<&str, Variant> = HashMap::from([
            ("urgency", Variant::U8(1)),
            ("desktop-entry", Variant::from(APP_ICON)),
        ]);
        let shown = match self.proxy().await {
            Ok(proxy) => {
                proxy
                    .notify(
                        APP_NAME,
                        replaces,
                        text.icon,
                        &text.summary,
                        &text.body,
                        text.actions,
                        hints,
                        text.expire,
                    )
                    .await
            }
            Err(e) => Err(e),
        };
        match shown {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!("no notification shown: {e}");
                None
            }
        }
    }

    pub async fn close(&self, server_id: u32) {
        let outcome = match self.proxy().await {
            Ok(proxy) => proxy.close_notification(server_id).await,
            Err(e) => Err(e),
        };
        if let Err(e) = outcome {
            // A notice that closed itself first is not a problem.
            tracing::debug!("cannot close notification {server_id}: {e}");
        }
    }

    /// Listens for presses on our notifications until the process ends; this client belongs to
    /// the task now. Every stop (a daemon restarting, or none there yet) is retried after a
    /// pause: a daemon that appears late, like plasmashell at login, still gets its listeners.
    pub async fn pump(self, events: UnboundedSender<Event>) {
        loop {
            match self.proxy().await {
                Ok(proxy) => self.listen(proxy, &events).await,
                Err(e) => tracing::debug!("no notification server yet: {e}"),
            }
            tokio::time::sleep(RETRY_DELAY).await;
        }
    }

    /// Awaits the two signals and the owner changes for as long as they come. Returns
    /// when a stream paused: the caller starts over, losing only the presses of the
    /// moment between.
    async fn listen(
        &self,
        proxy: proxies::NotificationsProxy<'static>,
        events: &UnboundedSender<Event>,
    ) {
        let mut invoked = match proxy.receive_action_invoked().await {
            Ok(invoked) => invoked,
            Err(e) => {
                tracing::debug!("no notification signals: {e}");
                return;
            }
        };
        let mut closed = match proxy.receive_notification_closed().await {
            Ok(closed) => closed,
            Err(e) => {
                tracing::debug!("no notification signals: {e}");
                return;
            }
        };
        let mut owners = match self.bus_watch().await {
            Ok(owners) => owners,
            Err(e) => {
                tracing::debug!("no owner watch of the notification server: {e}");
                return;
            }
        };
        loop {
            tokio::select! {
                pressed = invoked.next() => {
                    let Some(pressed) = pressed else { return };
                    if let Ok(args) = pressed.args() {
                        let _ = events.send(Event::Answered {
                            server_id: args.id,
                            key: args.action_key,
                        });
                    }
                }
                gone = closed.next() => {
                    let Some(gone) = gone else { return };
                    if let Ok(args) = gone.args() {
                        let _ = events.send(Event::Dismissed { server_id: args.id });
                    }
                }
                owner = owners.next() => {
                    let Some(owner) = owner else { return };
                    if let Ok(args) = owner.args()
                        && args.name.as_str() == SERVICE
                    {
                        // The daemon changed: the signals may come from another service now, and
                        // everything it showed is gone.
                        tracing::debug!("the notification server changed: listening again");
                        let _ = events.send(Event::DaemonChanged);
                        return;
                    }
                }
            }
        }
    }

    /// The bus watch for `NameOwnerChanged`. The bus never leaves mid-session, so this
    /// proxy outlives restarts of plasmashell.
    async fn bus_watch(&self) -> zbus::Result<fdo::NameOwnerChangedStream> {
        let dbus = fdo::DBusProxy::builder(&self.connection).build().await?;
        dbus.receive_name_owner_changed().await
    }
}
