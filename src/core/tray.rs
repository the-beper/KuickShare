//! The tray icon of the core: a StatusNotifierItem with the menu the shell used to build.
//!
//! The item lives in its own zbus connection managed by `ksni`, which registers with
//! `org.kde.StatusNotifierWatcher`. The state is a snapshot ([`View`]) that the task of
//! [`crate::desktop`] refreshes; the menu opens the window or changes the visibility, and
//! `ksni` calls those on its own thread, so every menu action goes through a channel.
//!
//! In a Flatpak sandbox the item may not take a well-known name
//! (`StatusNotifierItem-PID-ID`), so there the item registers with the connection's
//! unique name, as KStatusNotifierItem does.

use arc_swap::ArcSwap;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;

use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Category, Status, TextDirection, ToolTip};

use crate::i18n::{i18nc, i18ncp};

/// What one menu click (or a click on the icon) asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Open the window, on a named page when given ("settings").
    OpenWindow(Option<&'static str>),
    /// Open the window at the Send page with the file dialog open.
    OpenFiles,
    /// Open the window at the Send page and paste the clipboard.
    PasteClipboard,
    /// Toggle the persistent visibility.
    SetVisible,
    /// Toggle the 10 minutes visibility.
    ToggleTemporaryVisible,
    /// Quit the whole app.
    Quit,
}

/// What the icon and the menu show. A snapshot each time, so the tray is one
/// listener over the core state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub enabled: bool,
    /// The effective visibility: a temporary one overrides a vanished steady one,
    /// Do Not Disturb overrides both (see `Core::effective_visible`).
    pub visible: bool,
    /// A consent waits for an answer.
    pub waiting: bool,
    /// A transfer runs.
    pub busy: bool,
    /// Unix seconds the temporary visibility ends at (0 when none runs).
    pub visible_until: i64,
}

impl View {
    fn status(&self) -> Status {
        if !self.enabled {
            return Status::Passive;
        }
        if self.waiting {
            Status::NeedsAttention
        } else if self.busy || self.visible_until > 0 {
            Status::Active
        } else {
            // The icon does not stand out when there is nothing to report, as the HIG asks.
            Status::Passive
        }
    }
}

/// The icon over a shared snapshot ([`ArcSwap`] of [`View`]): `ksni` owns it in a thread of its
/// own and reads there; the task of [`crate::desktop`] swaps the latest state in and tells the
/// watcher to re-read it.
pub struct TrayIcon {
    view: Arc<ArcSwap<View>>,
    /// Where the menu clicks go.
    action: UnboundedSender<Action>,
}

impl TrayIcon {
    pub fn new(view: Arc<ArcSwap<View>>, action: UnboundedSender<Action>) -> Self {
        Self { view, action }
    }
}

impl ksni::Tray for TrayIcon {
    fn id(&self) -> String {
        "kuickshare".to_owned()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn title(&self) -> String {
        "KuickShare".to_owned()
    }

    fn status(&self) -> Status {
        self.view.load().status()
    }

    fn text_direction(&self) -> TextDirection {
        TextDirection::LeftToRight
    }

    /// The same name as the shell used, as a theme icon.
    fn icon_name(&self) -> String {
        "io.github.the_beper.KuickShare-symbolic".to_owned()
    }

    fn attention_icon_name(&self) -> String {
        "io.github.the_beper.KuickShare-symbolic".to_owned()
    }

    fn tool_tip(&self) -> ToolTip {
        let view = self.view.load();
        if view.visible_until > 0 {
            let left = view.visible_until - now();
            if left > 0 {
                let minutes: u64 = ((left + 59) / 60).max(1) as u64;
                let minutes_text = minutes.to_string();
                return ToolTip {
                    icon_name: "io.github.the_beper.KuickShare-symbolic".into(),
                    title: "KuickShare".into(),
                    description: i18ncp!(
                        "@info:tooltip",
                        "Visible for %1 more minute",
                        "Visible for %1 more minutes",
                        minutes,
                        &minutes_text
                    ),
                    ..ToolTip::default()
                };
            }
        }
        ToolTip::default()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // The menu of the shell followed the same flow: opening the window.
        let _ = self.action.send(Action::OpenWindow(None));
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        let _ = self.action.send(Action::OpenFiles);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let view = self.view.load();
        // Every `activate` runs later, on the ksni thread: it gets its own sender.
        let sender = |action: Action| {
            let sender = self.action.clone();
            Box::new(move |_: &mut Self| {
                let _ = sender.send(action);
            }) as Box<dyn Fn(&mut Self) + Send>
        };
        vec![
            MenuItem::Standard(StandardItem {
                label: i18nc!("@action:inmenu", "Open KuickShare"),
                icon_name: "window-new".into(),
                activate: sender(Action::OpenWindow(None)),
                ..StandardItem::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: i18nc!("@action:inmenu", "Send Files…"),
                icon_name: "document-send".into(),
                activate: sender(Action::OpenFiles),
                ..StandardItem::default()
            }),
            MenuItem::Standard(StandardItem {
                label: i18nc!("@action:inmenu", "Send Clipboard"),
                icon_name: "edit-paste".into(),
                activate: sender(Action::PasteClipboard),
                ..StandardItem::default()
            }),
            MenuItem::Separator,
            MenuItem::Checkmark(CheckmarkItem {
                label: i18nc!("@action:inmenu", "Visible to Nearby Devices"),
                icon_name: "visibility".into(),
                checked: view.visible,
                activate: sender(Action::SetVisible),
                ..CheckmarkItem::default()
            }),
            MenuItem::Checkmark(CheckmarkItem {
                label: i18nc!("@action:inmenu", "Visible for 10 Minutes"),
                icon_name: "chronometer".into(),
                checked: view.visible_until > 0,
                activate: sender(Action::ToggleTemporaryVisible),
                ..CheckmarkItem::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: i18nc!("@action:inmenu", "Preferences"),
                icon_name: "configure".into(),
                activate: sender(Action::OpenWindow(Some("settings"))),
                ..StandardItem::default()
            }),
            MenuItem::Standard(StandardItem {
                label: i18nc!("@action:inmenu", "Quit"),
                icon_name: "application-exit".into(),
                activate: sender(Action::Quit),
                ..StandardItem::default()
            }),
        ]
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
