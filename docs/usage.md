# KuickShare user guide

KuickShare sends and receives files, text and links with Android phones through Google's Quick Share, from a KDE Plasma 6 desktop. This guide covers installation, daily use, every preference and troubleshooting. For how it works inside, see [architecture.md](architecture.md); for the D-Bus interface of the background service, see [dbus-api.md](dbus-api.md).

KuickShare has two programs. `kuickshare` is the window, tray icon and notifications. `kuickshared` is the background service that speaks the Quick Share protocol; the window starts it and stops it.

## In this guide

- Setup: [Requirements](#requirements), [Installing, updating and uninstalling](#installing-updating-and-uninstalling), [First run](#first-run)
- Daily use: [Sending](#sending), [Receiving](#receiving), [Without a shared network](#without-a-shared-network-experimental), [Transfers page and history](#transfers-page-and-history)
- Devices: [Paired devices](#paired-devices), [Trusted devices](#trusted-devices)
- Reference: [Preferences](#preferences), [Do Not Disturb](#do-not-disturb), [Keyboard shortcuts](#keyboard-shortcuts), [Troubleshooting](#troubleshooting), [Where things are stored](#where-things-are-stored)

## Requirements

- KDE Plasma 6. The AppImage and the Flatpak bring their own libraries (the Flatpak needs Flatpak and [Flathub](https://flathub.org/setup)). The tarball uses the system's Qt 6 (6.9 or newer) and KDE Frameworks 6: on Fedora, `kf6-kirigami`, `kf6-kirigami-addons` and `qt6-qtdeclarative`. The installer checks for them and prints the `dnf install` (or `pacman`) line if one is missing.
- The computer and the phone on the same Wi-Fi network. Bluetooth finds devices and carries data only with the experimental [Wi-Fi upgrade](wifi-direct.md) on (the start of a transfer, slowly, until the Wi-Fi link takes over). Without a shared network, enable that upgrade.
- A Google account is not needed and cannot be used.

## Installing, updating and uninstalling

### Flatpak

```sh
flatpak remote-add --user --if-not-exists kuickshare https://the-beper.github.io/KuickShare/kuickshare.flatpakrepo
flatpak install --user kuickshare io.github.the_beper.KuickShare
```

The software center can add the repository from the same `.flatpakrepo` file. Updates come with `flatpak update` or the software center. A `.flatpak` bundle is also attached to each release (`flatpak install --user KuickShare-x86_64.flatpak`).

| Command | Effect |
|---|---|
| `flatpak run io.github.the_beper.KuickShare` | Start the app (also in the application menu) |
| `flatpak uninstall io.github.the_beper.KuickShare` | Remove the program |
| `flatpak uninstall --delete-data io.github.the_beper.KuickShare` | Also delete settings, history, paired devices and the identity key (`~/.var/app/io.github.the_beper.KuickShare`) |

The sandbox is deliberately open where the integration needs it:

| Permission | Why |
|---|---|
| Network | Quick Share and the pairing port |
| `org.bluez` (system bus) and Bluetooth sockets | Finding devices over Bluetooth, and the Bluetooth channel of a send |
| `org.freedesktop.NetworkManager`, `org.fedoraproject.FirewallD1` (system bus) | The [Wi-Fi upgrade](wifi-direct.md) (off unless enabled) |
| `org.kde.kuickshare` (own name) | Single-instance service of the window |
| `org.kde.JobViewServer`, `org.freedesktop.Notifications`, `org.kde.StatusNotifierWatcher` | Job progress, Do Not Disturb, tray icon |
| Downloads folder | Received files. Another download folder is refused with a message until you grant it: `flatpak override --user --filesystem=/path io.github.the_beper.KuickShare` |
| `~/.local/share/kio/servicemenus` and `~/.config/autostart` (create) | The app writes the Dolphin entry and the autostart entry there, since Flatpak cannot export them |

The Dolphin and autostart entries:

- Both start the app through `flatpak run`.
- The app finds the host folders from where Flatpak mounts the two granted ones, so a custom `XDG_DATA_HOME` or `XDG_CONFIG_HOME` works (tested in a container). Without such a mount it falls back to `~/.local/share` and `~/.config`.
- The AppImage and the Flatpak write the same Dolphin entry file, so install only one.
- Uninstalling the Flatpak leaves both behind: remove `kuickshare-send.desktop` and `io.github.the_beper.KuickShare.desktop` from those folders.

Files sent from Dolphin, the file chooser or by drag and drop reach the sandbox through the document portal. The hand-over of a file from a second launch is tested in a container; the file chooser and Dolphin are unverified on a real desktop.

### AppImage

A normal, self-contained AppImage: it brings its own libraries and runs on any distribution, including NixOS and musl ones. Running it starts KuickShare and installs nothing. The latest release is always at `https://github.com/the-beper/KuickShare/releases/latest/download/KuickShare-x86_64.AppImage`.

```sh
chmod +x KuickShare-x86_64.AppImage
./KuickShare-x86_64.AppImage
```

It can be managed with the usual tools ([Gear Lever](https://github.com/mijorus/gearlever), [AM / AppMan](https://github.com/ivan-hc/AM), [soar](https://github.com/pkgforge/soar)) and updates with `appimageupdatetool` or any updater that reads its embedded update information (zsync, GitHub releases).

On the first start it asks once whether to add KuickShare to the application menu and Dolphin's context menu:

| Answer | Effect |
|---|---|
| Add | Writes the menu entry and the Dolphin entry, both pointing at the AppImage file. If you move the file, turn the Dolphin entry off and on again in Preferences |
| Not now | Turns the Dolphin entry off in Preferences |

- The Preferences have an "Application menu entry" switch (AppImage only) that adds or removes the menu entry at any time, so it also works where the question is not asked.
- Nothing is asked if an installation or those entries already exist, if the app starts with `--background`, or if `XDG_CURRENT_DESKTOP` is not set (a bare session such as Xvfb).
- The answer is remembered in `~/.config/kuickshare/integration-asked`.
- To remove the menu entry, delete `~/.local/share/applications/io.github.the_beper.KuickShare.desktop` and the icons it installed.

| Command | Effect |
|---|---|
| `KuickShare-x86_64.AppImage [file...]` | Start the app; the files are queued on the Send page |

To install KuickShare into `~/.local` with the system's libraries instead, use the tarball (below).

### Tarball

For people who want neither the AppImage nor the Flatpak. It holds the program built against the system's Qt 6 and KDE Frameworks 6, and an installer that copies it into `~/.local`; afterwards the extracted folder can be deleted.

```sh
tar xf KuickShare-x86_64.tar.gz
KuickShare-<version>/install.sh
```

| Command | Effect |
|---|---|
| `install.sh [--no-launch] [file...]` | Install or update, then start the app (`--no-launch` only installs; a running instance is still restarted); the files are queued on the Send page |
| `install.sh --uninstall` | Remove the program; settings, history and paired devices stay |
| `install.sh --uninstall --purge` | Also delete settings, history, paired devices and the identity key |
| `~/.local/share/kuickshare/uninstall.sh [--purge]` | The same, without the tarball |

What the tarball installer writes, under `XDG_DATA_HOME` and `XDG_CONFIG_HOME` when set; the table shows the defaults:

| What | Where |
|---|---|
| Programs `kuickshare`, `kuickshared` | `~/.local/share/kuickshare/bin/` |
| Command in the shell path | `~/.local/bin/kuickshare` (a link; an existing file or link of yours is left alone) |
| Installer, uninstaller, version | `~/.local/share/kuickshare/` |
| Menu entry, app metadata | `~/.local/share/applications/`, `~/.local/share/metainfo/` |
| Dolphin "Send with KuickShare" | `~/.local/share/kio/servicemenus/kuickshare-send.desktop` |
| Notification definitions | `~/.local/share/knotifications6/kuickshare.notifyrc` |
| Icons | `~/.local/share/icons/hicolor/` |
| Translations | `~/.local/share/locale/` |
| Autostart entry (only if "Start at login" is on) | `~/.config/autostart/io.github.the_beper.KuickShare.desktop` |

Uninstalling removes all of these except the data listed under [Where things are stored](#where-things-are-stored), which `--purge` removes as well.

## First run

The window opens on the Send page and a tray icon appears. The computer is visible to nearby devices under the hostname, until you change the name in Preferences.

The window has a fixed size. Closing it hides it in the tray and frees its memory (queued files and the current page are forgotten); the service keeps running, so the computer can still receive. The tray icon opens the window on a left click. Its menu has these entries:

| Entry | Effect |
|---|---|
| Open KuickShare | Show the window |
| Send files... | Open the file chooser on the Send page |
| Send clipboard | Add the clipboard to the Send page |
| Visible to nearby devices | Same switch as in Preferences |
| Visible for 10 minutes | Visible even when the switch above is off, then back to it. Shows the time left while active; click again to stop |
| Preferences | Open Preferences |
| Quit | Quit at once; if a transfer is running, opens the window to ask first |

Without the tray icon (a preference), closing the window quits the app. See [Preferences](#preferences).

The tabs are Send and Transfers. The icon at their right opens a menu with Preferences, Shortcuts, Help, About and Quit.

## Sending

![The Send page with a device list](screenshots/send.png)

The Send page takes files, or text, or a link. Choose what to send, then click a device in the "Send to" list. While a transfer is running (sending or receiving, not one that only waits for your approval) the devices are greyed out and a note says so; you can still queue files and text, and send when it ends. The status line at the top says whether this computer is ready, hidden or hidden by Do Not Disturb.

On the phone, open Quick Share and set it to receive from **Everyone** first. See [Troubleshooting](#troubleshooting).

Before sending to a device found on the network, KuickShare checks briefly that it answers. If the phone is gone (for example its Wi-Fi went off, which it does not announce), its entry disappears and, with the Wi-Fi upgrade on and exactly one match by name and device type, the send goes over Bluetooth; otherwise it stops with "The device is not reachable". The other way round, a phone listed over Bluetooth that answers on the network is reached over the network. The route rules and timings are in [wifi-direct.md](wifi-direct.md#behaviour). The transfer row keeps the entry you clicked. **Cancel** is available from the moment you press a device ("Looking for the device…", "Connecting…", "Waiting for the other device to accept").

### Files

![The Send page with files queued](screenshots/send-files.png)

There are five ways to add files, and they combine:

- Click the drop area, or press `Ctrl+O`, or use **Send files...** in the tray menu.
- Drop files on the drop area, or anywhere on the window. The window accepts a drop on any page and switches to Send.
- Press `Ctrl+V` on the Send page. File paths copied in Dolphin are added as files. An image on the clipboard without text, such as a screenshot, is saved as a PNG in `~/.cache/kuickshare/clipboard/` and added.
- Use **Paste from clipboard** in the drop area, or **Send clipboard** in the tray menu.
- Pass them on the command line, or use the Dolphin entry below.

Only regular files are queued; folders, given on the command line or dropped on the window, are ignored. The list shows the first five files and a "Show all" link for the rest. **Clear** empties the list, **Add** adds more.

### Text and links

The "Text or link" box takes up to 100000 characters. A counter appears when the text is within 10% of the limit; pasting and typing stop at the limit and longer text is cut. Spaces and blank lines around the text, and trailing spaces on each line, are removed before sending.

If the whole text is one link of the form `scheme://host.tld...`, it is sent as a link by default. The receiving phone then offers to open it. Tick **Send as text** to send it as plain text instead. A link with any other text around it can only be sent as text, so the box is ticked and locked. Files and text are exclusive: if files are queued, they are sent and the text is not.

`Ctrl+V` outside the text box pastes into the page as described above; inside the text box it pastes text only.

### From Dolphin and the command line

Right-click files in Dolphin and choose **Send with KuickShare**. It is a top-level entry and opens the Send page with the files queued. It can be turned off in Preferences.

```sh
kuickshare [--background] [--page send|transfers|settings|help] [--device <name>] [files...]
```

`kuickshare --help` lists the options. If KuickShare is already running, a second launch hands its files to the running window and shows it. `--page` accepts `send`, `transfers`, `settings` or `help`; the window on the Send page is the default. `--background` starts hidden in the tray, which is what the login entry uses.

`--device <name>` sends the files to the nearby device with that name, without a click: the name is compared whole, ignoring case and surrounding spaces. Discovery runs for up to 30 seconds, and a match must stay the only one for about 3 seconds before the send. On a match the Transfers page opens (with `--background` the app stays in the tray). If no device or several devices have that name, or there are no files (none given and none queued on the Send page), nothing is sent, the files stay queued on the Send page and the window opens to show the error, also with `--background`. A new `--device` request cancels a pending one, and files added meanwhile by a plain launch are not part of its send.

## Receiving

When a nearby device wants to send, a persistent notification titled "Incoming transfer" names the sender, shows a PIN if the protocol supplied one, and offers these actions:

| Action | Effect |
|---|---|
| Accept | Receive the transfer |
| Accept and trust | Accept, and add the sender to the trusted devices (only if trusted devices are on; see [below](#trusted-devices)) |
| Decline | Refuse the transfer |

The same request is also listed on the Transfers page with Accept and Decline buttons. Compare the PIN with the one on the phone before accepting.

A request nobody answers is declined after 5 minutes.

Files go to the download folder, `~/Downloads` unless you chose another one in Preferences. Progress and a cancel button appear in Plasma's job tracker. When it finishes you get a "Files received" notification with **Open folder**. Text arrives as a "Text received" notification with **Copy**, and **Open link** when the text is a single `http://` or `https://` address.

A sender can propose file names that would write outside the download folder. KuickShare declines such a transfer before asking you. A name is refused if it is empty, is `.` or `..`, is longer than 255 bytes, or contains a `/`, a `\`, a control character, or an invisible character such as a zero-width space or a right-to-left override. The decision is logged and no notification is shown; how the Transfers page then shows the row was not checked (unverified).

**Wi-Fi networks.** When a phone shares a Wi-Fi network, the finished transfer's row shows its name and a **Join network** button. After you confirm, KuickShare connects this computer through NetworkManager and saves the network, so it connects again by itself; the current Wi-Fi connection is replaced. Only WPA/WPA2 (a passphrase, not a 64-hex key) and open networks work, and not names starting with `kuickshare-`. If you already have a connection for that network, a second one is added. The password is never written to disk; it is kept in memory for 10 minutes at most, so the button stops working after that, after the transfer is declined, or when the sharing service restarts (the error says so); rows from earlier sessions have no button. It is refused while another transfer or a Wi-Fi upgrade is running. Not tried against a real NetworkManager or phone *(inferred)*.

Sending to the phone produces the same notifications in the other direction. If the phone declines before any data moved, you see "Transfer not accepted"; a lost connection gives "Transfer interrupted".

## Without a shared network (experimental)

With **Settings → Wi-Fi Direct → Receive and send without a shared network** on, a phone whose Wi-Fi is off or on another network can send to this computer, and this computer can send to it. The transfer starts over Bluetooth and moves to a temporary Wi-Fi link. It is off by default.

| | Receiving | Sending |
|---|---|---|
| Phone | Quick Share open on the share sheet | Quick Share's receive screen open, visibility **Everyone** |
| Computer | Visible; the setting on | The phone appears with a Bluetooth icon on the Send page. The refresh button next to "Send to" scans again (a scan lasts about 15 s and is never continuous) |
| Wi-Fi link | This computer starts a hotspot | The phone starts a Wi-Fi Direct group and this computer joins it |
| Needs | The `dnsmasq` package: NetworkManager runs it to give the phone an address | Nothing extra |

**Requirements and firewall**

- NetworkManager and Bluetooth; no privileges.
- With firewalld the computer opens only the port of the transfer.
- Without firewalld (ufw, nftables or none) the Wi-Fi hotspot is put in a zone that lets the phone reach every service of the computer, so turning the setting on asks you to confirm.
- A computer with one radio can lose its Wi-Fi connection while the link is up.

**Visibility**

- While the setting is on, this computer is visible over Bluetooth, under its device name, to devices in range. Nothing is sent without your consent.
- A phone on the same network as the computer is reached over the network and never takes part in the upgrade; it is listed once, even if Bluetooth sees it too.

**Trust and PIN**

- "Accept and trust" is not offered for Bluetooth transfers.
- A PIN is shown for each one: compare it with the phone.

**Network changes** (need NetworkManager and the setting on)

- When the last network connection drops (for example the Wi-Fi is turned off), the devices found over the network disappear from the Send page at once and a Bluetooth scan starts. A phone still in range shows up with the Bluetooth icon after a few seconds.
- When a network comes back, the search for devices starts over at once, so a phone on it is found without waiting for the periodic check (up to about 30 s).

**Plasma and the job tracker**

- Plasma shows "Connection 'kuickshare-ap' activated" for the temporary link. It cannot be hidden per connection, only for all networks in System Settings → Notifications → Network Management.
- When you press a device on the Send page, the Transfers page gets its row and a job appears in Plasma's job tracker at once. The job names the step, in this order: "Looking for the device…", "Connecting…", "Waiting for the other device to accept", then "Switching to Wi-Fi…" until data moves.
- When it finishes, the Send page is left empty; after a failure the files and text stay for a new attempt.

## Transfers page and history

![The Transfers page](screenshots/transfers.png)

The Transfers page lists transfers newest first, running ones included. A card shows the device, the state, the size and the date. While a transfer runs, the state line also shows the speed and the time left once there is an estimate. Buttons depend on the state:

| Button | When |
|---|---|
| Accept, Decline | A request is waiting |
| Cancel | A transfer is running |
| Open folder | A received file transfer finished |
| Copy | A text or link was received or sent |
| More details | Always on finished entries: result, size, file names or text. For files received in this session (the saved history does not keep where files went), each file still in the download folder has an **Open** button; executables and launcher files never do |
| Remove from list | Finished entries |

At the top, a search field and two menus narrow the list: the search matches the device name, received text and file names; the menus pick the direction (all, received, sent) and the status (any, in progress, completed, not completed, which covers cancelled, declined and interrupted transfers). The filters are not saved and do not change the history. With no match the page says so; **Reset filters**, next to the menus, brings the whole list back (it is greyed out while no filter is set).

When a transfer ends, a message slides up from the bottom edge of the window, on any page, and stacks above older ones (at most four):

| Message | Colour | Stays |
|---|---|---|
| Transfer completed | Green | 4 s |
| The transfer failed | Red | 10 s |
| The transfer was declined, The transfer was cancelled | Blue | 6 s |

Errors the service reports, and "Copied to clipboard" (green, 1.5 s), use the same messages. A message that stays more than 3 s has a close button.

Entries keep at most 200 file names and 10000 characters of text. Wi-Fi credentials received from a phone are saved without the password; the password is never written to disk (it stays in memory for 10 minutes at most, see [Receiving](#receiving)). **Clear history** deletes the whole list, not only the transfers the filters show, and does not touch received files. A banner suggests clearing when the history file gets large.

The history is saved in a file on disk, 1000 entries at most (the oldest go first). It is controlled in Preferences: it can be turned off, and entries can expire after 1, 7, 30 or 90 days. With the history off, the page only lists transfers made while the app is open.

## Paired devices

Pairing lets two computers running KuickShare send to each other without asking. Phones cannot take part, because the pairing uses a channel of its own on top of Quick Share (see [architecture.md](architecture.md) for the protocol and its limits).

![The pairing dialog showing an 8-digit code](screenshots/pair.png)

1. On both computers open Preferences and choose **Pair a device...**.
2. On one of them, click the other computer in the list. Computers already paired are listed as "Already paired" and cannot be chosen.
3. Both screens show the same 8-digit code. Check that it is identical, then click **The codes match** on both. If it differs, click **They differ**.
4. Each computer shows "Paired with ..." when the other side confirmed too.

Closing the dialog before confirming cancels the pairing.

The computers must reach each other on TCP port **47891**; open it in the firewall if one is active. Paired computers are listed in Preferences with a short fingerprint and a **Remove** button. Removing one only forgets it on this computer; remove it on the other computer too.

Once paired, a transfer from the other computer is accepted without a notification when it arrives from the same IP address shortly after the other computer announced it (limits in [architecture.md](architecture.md#limits-on-the-trust-port)). The match is by address, so pair only computers you own.

## Trusted devices

Phones cannot pair, so KuickShare can remember them instead. Tap **Accept and trust** on the notification of an incoming transfer, and that device is accepted without asking from then on.

A device is recognised by its advertised name **and** its MAC address together. The MAC is read from the kernel's ARP table for the sender's IP address, so this works on the local network only. If the MAC cannot be read, you are asked as usual. Android uses a different MAC for each Wi-Fi network, so the same phone on another network is asked about again.

> [!WARNING]
> Someone on your network who copies both the name and the MAC is accepted too. Use trusted devices only on networks where you trust every device.

Preferences has a **Trusted devices** section with the list (name and MAC address, each with **Remove**) and a **Trust devices** switch. Switched off, listed devices are asked about again and the notification has no **Accept and trust** action. The list is kept for when the switch is turned on again.

## Preferences

![The Preferences page](screenshots/preferences.png)

Open it with `Ctrl+,`, from the menu, or from the tray. Changes apply at once and are saved in `settings.json`.

| Section | Preference | Default | Effect |
|---|---|---|---|
| Visibility | Device name | Hostname | Name shown to other devices. Changing it restarts sharing; it fails while a transfer runs |
| Visibility | Visible to nearby devices | On | Others can find this computer and send to it. Off means nothing can be received |
| Visibility | Hide during Do Not Disturb | On | Hidden while Plasma's Do Not Disturb is on. See [Do Not Disturb](#do-not-disturb) |
| Receiving | Download folder | `~/Downloads` | Where received files are saved. "Use the default folder" resets it |
| Receiving | Random port / Fixed port | Random | The port for transfers. Random picks a new one at every start; a fixed one (1024 to 65535) can be opened in the firewall. Changing it restarts sharing and fails while a transfer runs |
| Paired devices | Pair a device... | none paired | Opens the [pairing dialog](#paired-devices); lists paired computers |
| Trusted devices | Trust devices | On | Accept [trusted devices](#trusted-devices) without asking; lists them |
| History | Save transfer history | On | Record finished transfers |
| History | Delete entries after | Never | Never, 1, 7, 30 or 90 days. Applied at startup, when the history is read and when a transfer is added |
| Startup | Start at login | Off | Writes the autostart entry; the app starts hidden. The entry points at the running binary, so switch it off and on again after moving the installation |
| Integration | Show tray icon | On | Without it, closing the window quits the app and nothing can be received until you open it again. At login it still starts hidden, so start KuickShare again to see the window |
| Integration | Dolphin context menu | On | Adds or removes **Send with KuickShare** |
| Wi-Fi Direct | Receive and send without a shared network | Off | Experimental; see [Without a shared network](#without-a-shared-network-experimental) |
| Reset | Reset preferences... | | Restores every setting to its default, including autostart off and the Dolphin entry on. History, paired and trusted devices are kept |

## Do Not Disturb

When "Hide during Do Not Disturb" is on and Plasma's Do Not Disturb is active, KuickShare hides this computer from nearby devices. The Send page shows "Do Not Disturb" and "Hidden until Do Not Disturb ends". The visibility preference itself is not changed, so the computer reappears when Do Not Disturb ends. The state is read from the notification service and re-read if the Plasma shell restarts.

KuickShare only changes the advertised visibility and cancels nothing; whether the library drops a transfer already running was not checked (unverified).

## Keyboard shortcuts

Also listed in the app under More, Shortcuts.

| Shortcut | Action |
|---|---|
| `Ctrl+1` | Send page |
| `Ctrl+2` | Transfers page |
| `Ctrl+F` | On the Transfers page: go to the search field |
| `Ctrl+,` | Preferences |
| `Ctrl+H`, `F1` | Help |
| `Ctrl+?` | Show the shortcuts |
| `Ctrl+O` | Choose files to send |
| `Ctrl+V` | On the Send page: paste files, an image or text from the clipboard |
| `Ctrl+L` | Clear files and text |
| `Ctrl+W` | Close the window (hides it, or quits without the tray icon) |
| `Ctrl+Q` | Quit KuickShare |

## Troubleshooting

**The phone does not see this computer.** Check, in this order:

1. "Visible to nearby devices" is on in Preferences and the Send page says "Ready".
2. Do Not Disturb is off, or "Hide during Do Not Disturb" is off.
3. Both devices are on the same network, and the network allows devices to talk to each other. Guest and some mesh networks do not.
4. The firewall lets mDNS through (UDP 5353). If you fixed a transfer port in Preferences, open that TCP port too. With a random port the firewall has to allow incoming connections from the local network. On Fedora, `firewalld` is the usual cause.

**This computer does not see the phone.** The phone must be set to **Everyone** and its Quick Share screen should stay open. A phone that is hidden or set to contacts announces no name and is not listed. Android sets the visibility back after 10 minutes, so set it again when the list goes empty.

**The red banner "The sharing service is not running".** The window could not reach `kuickshared`. Click **Start service** in the banner. If it does not come back, start `kuickshare` from a terminal to see the service's output.

**A trusted phone is asked about again.** It joined another network and has a new MAC, or its name changed. Accept and trust it again.

**A phone stays in the list after its Wi-Fi went off, or sending ends with "The device is not reachable".** The phone left the network without saying so. Pressing it removes the entry. Turn its Wi-Fi on, or open Quick Share on it so it appears again. Over the network a send gives up after a short wait ([timings](wifi-direct.md#timings-and-timeouts)) and reports the failure.

**Transfer not accepted.** The phone declined, timed out, or could not receive (for instance, a full disk).

### Where things are stored

| What | Location |
|---|---|
| Settings and trusted devices | `~/.config/kuickshare/settings.json` |
| History | `~/.local/share/kuickshare/history.json` |
| Identity key (mode 600) | `~/.local/share/kuickshare/identity.json` |
| Paired computers | `~/.local/share/kuickshare/peers.json` |
| The same, in the Flatpak | `~/.var/app/io.github.the_beper.KuickShare/config/kuickshare/` and `.../data/kuickshare/` |
| Logs | No log file. The service writes to the standard output of the window process, so they show in a terminal when you run `kuickshare` there. Where the desktop session keeps them otherwise is unverified |

A settings or peers file that cannot be parsed is moved to a `.bak` copy next to it and defaults are used. An unparsable identity file stops the service from starting; it is never replaced. An unparsable history file is ignored and logged. The log level can be raised for one run: `RUST_LOG=debug kuickshare`, since the service inherits the environment.
