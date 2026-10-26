# Architecture

How KuickShare is put together and what it does and does not protect against. For the D-Bus members see [dbus-api.md](dbus-api.md); for day-to-day use see [usage.md](usage.md); for reporting a vulnerability see [SECURITY.md](../SECURITY.md).

- [Process model](#process-model)
- [Transfer data flow](#transfer-data-flow)
- [Transfer lifecycle](#transfer-lifecycle)
- [Files on disk](#files-on-disk)
- [Trust model](#trust-model)
- [Threat model](#threat-model)
- [Display of remote names](#display-of-remote-names)
- [Testing](#testing)
- [Packaging](#packaging)

## Process model

Two processes talk over the session D-Bus (`io.github.the_beper.KuickShare`, interface `...KuickShare1`).

| Process | Source | Language | Job |
| --- | --- | --- | --- |
| `kuickshared` (core) | `src/core/` | Rust, tokio, zbus | Quick Share protocol through `rqs_lib` (vendored path dependency, see [THIRD_PARTY.md](../THIRD_PARTY.md)), discovery, trust channel, settings, history |
| `kuickshare` (shell) | `src/shell/` | C++20, Qt 6, KF 6, Kirigami | Window, tray, notifications, progress jobs, Dolphin entry, Do Not Disturb |

The shell creates its window and QML engine only while the window is shown and destroys them on hide, so a hidden shell holds just the tray and the core connection. The split keeps the protocol code in Rust, where the library lives, and the desktop integration in Qt/KF6, where the bindings are mature. It also lets the core run and be tested without any UI (see [Testing](#testing)). The shell holds no protocol state: after a reconnect it reloads everything from the core.

```mermaid
flowchart LR
    subgraph shell["kuickshare (shell)"]
        QML["QML UI + tray"]
        CB["CoreBridge"]
        TW["TransferWatcher"]
        DW["DndWatcher"]
        QML --- CB
        TW --- CB
        DW --- CB
    end
    subgraph core["kuickshared (core)"]
        SVC["D-Bus service"]
        CORE["Core: settings, history"]
        RQS["rqs_lib: Quick Share + mDNS"]
        TR["Trust: Noise XX listener"]
        SVC --- CORE
        CORE --- RQS
        CORE --- TR
    end
    CB <-->|"session D-Bus: methods, properties, signals"| SVC
    TW -->|"KNotification, KUiServerV2"| NS["Plasma notifications"]
    DW -->|"Inhibited property"| NS
    DOL["Dolphin service menu"] -->|"kuickshare FILES"| shell
    RQS <-->|"mDNS + TCP (random or static port)"| PHONE["Phone / other Quick Share device"]
    TR <-->|"TCP 47891, Noise XX"| PEER["Other KuickShare computer"]
```

### Who starts whom

- The shell starts the core. At startup `CoreBridge` checks whether the bus name is owned. If it is, the shell attaches to that core. If not, it runs `kuickshared` from its own directory with `QProcess`.
- The shell is single-instance (`KDBusService::Unique`). A second launch of the same version, for example from Dolphin, only queues its files in the running window.
- A launch of a different version, or with `--replace`, first asks the running shell to `quit()` over D-Bus (`src/shell/instance.cpp`), waits up to 8 s for it and 3 s for its core to leave the bus, then starts normally. An update therefore takes over a shell left in the tray. A transfer running in the old shell is lost.
- `UpdateWatcher` (`src/shell/updatewatcher.cpp`) notices an update installed under a running Flatpak (portal update monitor) or AppImage (the `$APPIMAGE` file replaced, after 5 s without changes). Once no transfer is active and the window is closed, `main.cpp` starts `kuickshare --replace` (Flatpak: portal `Spawn` with the latest version; AppImage: the file). It does so when the update arrives, when the window closes, every 30 s while waiting, and when the user opens the window from the tray icon or its "Open KuickShare" entry (then without `--background`, so the new version opens it). With the window open, a banner offers "Restart" (`CoreBridge::restartForUpdate`, disabled during a transfer), which starts the replacement the same way without `--background`. While a replacement is starting, further triggers are ignored for 15 s. The tarball installer restarts the app itself. Both watchers are unverified on a real desktop *(inferred from the portal specification)*.
- On exit the shell sends SIGTERM to a core it started and waits up to 3 s. A core it only attached to is left running. The core stops its engine on SIGTERM or Ctrl+C.
- Autostart is a `.desktop` file in `~/.config/autostart` that runs `kuickshare --background`.

### When the core is down

`CoreBridge` watches the bus name. When the owner disappears it marks itself disconnected and clears the device list, and the window shows an error banner with a "Start service" button. Nothing is restarted automatically. The button kills a core that is running but not on the bus, then starts a new one. When the name reappears the shell reloads properties, history, paired devices and the allowed list, and drops its transfer rows. A transfer in progress when the core died is lost.

### Inside the core

| Module | Role |
| --- | --- |
| `main.rs` | Loads settings, history and trust, serves D-Bus, starts the engine, waits for a signal |
| `app.rs` | `Core`: owns the `rqs_lib` engine and the settings. Settings that change the device name or port rebuild the engine, and the old values are restored if that fails |
| `dbus.rs` | The D-Bus interface, plus forwarder tasks that turn library events into signals, run the consent timeout, the name guard and auto-accept |
| `link.rs` | `NmLink`: the Wi-Fi link of the upgrade over NetworkManager's D-Bus (access point or join); see [Wi-Fi upgrade](wifi-direct.md) |
| `trust.rs` | Noise XX channel on TCP 47891: probe, announce, pairing |
| `allowed.rs` | Trusted devices by name and MAC |
| `safety.rs` | File name guard |
| `ratelimit.rs` | Per-address connection limit for the trust port |
| `history.rs`, `settings.rs` | JSON persistence |

Settings that restart the engine (device name, static port, reset) are refused while a transfer is active. A transfer with no event for 600 s counts as over, so a lost final event cannot block them forever.

### Inside the shell

| Part | Role |
| --- | --- |
| `main.cpp` | Window, tray, single instance, starts the core |
| `corebridge.*` | `CoreBridge`: D-Bus client and the QML-facing state; `transferBusy` is true while a transfer is live and not just awaiting approval |
| `transfermodel.*` | `TransferModel`: live and history rows; `beginPending`/`failPending` show a send before the core reports it; `busy` and `transferEnded` feed the Send page and the toasts |
| `transferwatcher.*`, `transferjob.*` | Notifications and the Plasma progress job of each transfer |
| `transferfiltermodel.*` | `TransferFilterModel`: `TransferModel` narrowed by direction, status group and search text for the Transfers page |
| `devicemodel.*` | Nearby devices |
| `dndwatcher.*` | Do Not Disturb state |
| `autostart.*`, `integration.*`, `dolphinmenu.*`, `launcher.h`, `execquote.h` | Autostart and Dolphin entries, first-launch question, `Exec` quoting, AppImage and Flatpak path detection |
| `qml/` | Pages (Send, Transfers, Settings, Help), dialogs, and `ToastStack.qml`: transient messages shown on whatever page is open |

## Transfer data flow

Transfer ids are `ip:port` of the remote side, or `ble:<endpoint>` for a phone found over Bluetooth. Event and method names below are shortened; see [dbus-api.md](dbus-api.md).

```mermaid
sequenceDiagram
    participant P as Remote device
    participant L as rqs_lib
    participant C as Core (dbus.rs)
    participant S as Shell
    participant U as User

    Note over P,U: Incoming
    P->>L: connection request, file names, size
    L->>C: event WaitingForUserConsent
    C->>C: unsafe file name? then decline
    C->>C: start consent timer
    C->>C: intent from paired peer, or allowed name+MAC? then accept
    C->>S: signal TransferUpdated
    S->>U: notification: Accept / Accept and trust / Decline
    U->>S: Accept
    S->>C: Accept(id)
    C->>L: ConsentAccept
    L->>P: receive files into the download folder
    L->>C: events ReceivingFiles ... Finished
    C->>C: write history entry
    C->>S: TransferUpdated (final, history_uid)

    Note over P,U: Outgoing
    U->>S: pick files and a device
    S->>C: SendFiles(device, paths)
    C->>C: check paths exist, look up device
    opt device is a paired KuickShare computer
        C->>P: announce over Noise (2 s limit)
    end
    C->>L: SendInfo
    L->>P: connection request
    P->>L: accepted or rejected
    L->>C: events SendingFiles ... Finished
    C->>S: TransferUpdated
```

An announce that fails only logs a message. The transfer still goes out and the receiver asks its user.

### Over Bluetooth (experimental)

With the Wi-Fi upgrade setting on, a phone that shares no network with the computer takes the same path through `rqs_lib`, `Core` and the shell; only the first channel and the move to Wi-Fi differ. Details and status: [wifi-direct.md](wifi-direct.md).

```mermaid
sequenceDiagram
    participant P as Phone
    participant L as rqs_lib
    participant C as Core
    participant N as NetworkManager

    Note over P,N: Incoming
    P->>L: finds the BLE advertisement, GATT connection
    L->>L: GATT bridge feeds a normal receive session
    P->>L: UKEY2, connection request (same events as above)
    L->>C: WaitingForUserConsent (always asks, no trust)
    L->>C: link.host() asks for a hotspot
    C->>N: open the port in firewalld, then AddAndActivateConnection2 (AP, IPv4 shared: dnsmasq serves DHCP)
    L->>P: UPGRADE_PATH_AVAILABLE (ssid, password)
    P->>L: joins, TCP connect, files continue there

    Note over P,N: Outgoing
    C->>L: ScanBluetooth, then SendInfo with a ble: id
    L->>P: rescan, L2CAP dial, UKEY2, connection request
    P->>L: accepts and offers WIFI_DIRECT
    L->>C: link.join(ssid, password)
    C->>N: connect as a WPA2 client
    L->>P: TCP connect, files continue there (plain frames)
```

If the move fails, the transfer stays on the Bluetooth channel (about 5 KB/s). The core reports a send that never starts within 40 s as a failed transfer (see [dbus-api.md](dbus-api.md#transfer-states)).

## Transfer lifecycle

The state names come from `rqs_lib` and are passed through unchanged as strings. The core only interprets a few.

| State | Meaning for KuickShare |
| --- | --- |
| `WaitingForUserConsent` (inbound) | Starts the consent timer; the only state where auto-accept is evaluated |
| `SendingFiles`, `ReceivingFiles` | Counts as active for the shell (progress job, Do Not Disturb, blocking settings) |
| `Finished`, `Cancelled`, `Rejected`, `Disconnected` | Final. The transfer leaves the active set and is written to the history |

- The first final state wins. `Finished` may be followed by `Disconnected`; the second is ignored in the core and in the shell's `TransferModel`.
- Later events often carry less data than earlier ones, so the core merges events and keeps known fields.
- The library reports a refusal by the remote side as `Disconnected`. The shell shows "not accepted" when an outgoing transfer ends that way before any data moved.
- The core's guard may decline an inbound transfer at any non-final event that lists file names, not only at the consent request.

## Files on disk

Paths follow the XDG variables; defaults are shown. Writes go to `<name>.tmp` and are renamed over the target.

| File | Location | Contents | If unreadable or invalid |
| --- | --- | --- | --- |
| `settings.json` | `~/.config/kuickshare/` | All preferences, including the allowed (trusted by name and MAC) list | Renamed to `settings.json.bak`, defaults used |
| `identity.json` | `~/.local/share/kuickshare/` | Long-term Noise key pair, mode 0600, created on first start | The core refuses to start; the file is not touched |
| `peers.json` | `~/.local/share/kuickshare/` | Paired computers: name, public key, SHA-256 fingerprint, pairing time | Renamed to `peers.json.bak`, no paired computers |
| `history.json` | `~/.local/share/kuickshare/` | Finished transfers, newest first | Logged and treated as empty; no `.bak`, so the next save overwrites it |
| Received files | Setting `download_dir`, else the library default (XDG download dir) | Written by `rqs_lib` | Not managed by KuickShare |
| Clipboard images | `<cache>/clipboard/` | PNGs saved to send a pasted image; the shell deletes those older than 1 h at startup | |
| Autostart entry | `~/.config/autostart/io.github.the_beper.KuickShare.desktop` | Written by the shell setting | |
| Installation | `~/.local/share/kuickshare/bin/`, `.desktop`, icons, `.mo`, service menu | See [Packaging](#packaging) | |

`--uninstall` keeps the first four rows; `--uninstall --purge` deletes them.

## Trust model

KuickShare takes part in ordinary Quick Share, where the sender is anonymous and every transfer needs the user's consent. On top of that it has two optional ways to skip the prompt. They are independent and can be switched off in the settings (trusted devices) or by not pairing.

| | Paired computer | Trusted device |
| --- | --- | --- |
| Works with | Another KuickShare installation | Any Quick Share sender, e.g. a phone |
| Identity | Noise static public key | Advertised name plus MAC address |
| Set up by | Pairing with a code compared on both screens | "Accept and trust" on a notification |
| Strength | Cryptographic for the announcement; the transfer itself is matched by IP | Weak: both values can be copied by a LAN attacker |

### Paired computers

The trust channel is a Noise XX handshake (`Noise_XX_25519_ChaChaPoly_BLAKE2s`, crate `snow`) over TCP 47891. The port is fixed so a peer can be reached knowing only its IP. The core listens on all interfaces. Both sides send their device name in the handshake payload. Frames are length-prefixed (2 bytes) and every read or write has a 10 s timeout. After the handshake the initiator sends one of three requests:

| Request | Effect |
| --- | --- |
| Probe | Answered for anyone. Says whether the caller's key is paired and whether the pairing window is open. Used during discovery to label cards "KuickShare" or "trusted" and to enable the card in the pairing dialog. That the window is open is therefore visible to anyone on the LAN, who could see it by trying to pair anyway |
| Intent | Accepted only from a paired key. Stores the peer's name for its IP for the intent lifetime ([limits](#limits-on-the-trust-port)), once |
| Pair | Accepted only while the user has opened the pairing window and the peer sent a non-empty name |

Pairing:

1. Both sides derive an 8-digit code (shown as `1234 5678`) from the first 4 bytes of the Noise handshake hash. A man in the middle would have to find a key pair that gives the same code on both sides; the source comment says 6 digits could be searched in under a minute and 8 digits cannot in the time the user has (estimate by the author, unverified).
2. Each user compares the codes and confirms or cancels within the confirmation time ([limits](#limits-on-the-trust-port)).
3. Each side sends its answer. Only if both said yes does each store the other's public key in `peers.json`.

Only one pairing may run at a time, so another host cannot take over the code on screen. Two computers count as trusted for sending only if each has the other's key; the probe checks both directions.

Receiving: when an inbound transfer asks for consent, the core looks for a still valid intent from the same source IP. If there is one, it accepts. The match is by IP address, not by key, so the transfer itself is not authenticated to the Noise identity (see [SECURITY.md](../SECURITY.md)).

Sending: the core announces only to computers it saw as `trusted` in a probe.

### Trusted devices by name and MAC

"Accept and trust" on a consent notification stores the sender's name and its MAC address in the allowed list. The MAC is read from the kernel ARP table (`/proc/net/arp`) for the transfer's source IP. This works only for IPv4 neighbours on the same link; an incomplete entry, another network or IPv6 gives no MAC, and the device is then asked about normally.

A later transfer is accepted without asking if the option is on, the name is non-empty and both name and MAC match an entry. The MAC is looked up only when the option is on.

> [!WARNING]
> Name and MAC are both set by the sender. Anyone on the same network who copies them is accepted without a prompt. The file name guard still applies, but the files are written to the download folder. Use this only on networks you trust.

### Consent and the guard

- **Consent timeout.** An inbound transfer waiting for an answer is declined after 300 s.
- **File name guard** (`safety.rs`). The library writes the names it is given straight into the download folder, so the core declines any inbound transfer with a name that is empty, `.` or `..`, longer than 255 bytes, or contains `/`, `\`, a control character or an invisible or direction-changing character (U+200B to U+200F, U+202A to U+202E, U+2060 to U+2064, U+2066 to U+2069, U+FEFF). It runs before consent and before auto-accept, so no rule can bypass it.

### Limits on the trust port

| Limit | Value |
| --- | --- |
| Connections per address | 30 per 60 s; refused attempts do not extend the block |
| Open connections per address | 4 |
| Open connections in total | 32 |
| Idle time per read or write | 10 s |
| Intent lifetime | 60 s, used once |
| Pairing window | 5 min, opened by the user |
| Pairing confirmation | 120 s |

Refused connections are closed without a reply.

### History caps

| Cap | Value |
| --- | --- |
| Files kept per entry | 200 |
| Text kept per entry | 10,000 characters |
| Age | `history_keep_days` (0 keeps everything); applied on load, on add and on read |
| Number of entries | 1000 (`MAX_ENTRIES` in `src/core/history.rs`), the oldest go first; applied on load, on add and on read |
| Off switch | `history_enabled`; when off nothing is recorded |

Wi-Fi credentials are recorded by network name only, never the password.

### What the core never does

- Accept a transfer with an unsafe file name, or with no consent, intent or allowed-list match.
- Pair without the window open, with an unnamed computer, or without both users confirming.
- Connect to a non-local address because an mDNS advert named one. Probes go only to private, link-local and loopback addresses, at most once per 20 s per address.
- Store a Wi-Fi password, or send the private key anywhere.
- Open links or run received files. The shell opens a link only when the user clicks.
- Read the contents of the files being sent, apart from handing their paths to the library.

What `rqs_lib` itself does on the network is not audited here (unverified).

## Threat model

Assumes the attacker is on the same LAN, or a malicious app runs as the same user. A passive observer of the Quick Share connection is a protocol matter and out of scope.

| Threat | Mitigation | Residual risk |
| --- | --- | --- |
| Sender uses a path in a file name to write outside the download folder | Name guard declines the transfer | Names that are valid but misleading (long, unusual Unicode) still pass |
| Sender hides the real extension with direction characters | Guard rejects them in file names; the shell neutralises markup in names | The shell does not strip direction characters from device names |
| Unknown sender floods the user with requests | Each request needs consent and expires after the consent timeout | No limit on the number of requests (unverified for the library) |
| Partial files, no disk space check, any size | None in KuickShare | Documented in [SECURITY.md](../SECURITY.md) |
| Man in the middle during pairing | Noise XX plus an 8-digit code compared by both users | The user confirms without comparing |
| Stranger starts a pairing | Pairing window must be opened by the user, expires, one at a time, needs both confirmations | While open, anyone on the LAN can make a code appear; the user must decline |
| Copy of name and MAC of a trusted device | MAC from the ARP table, same link only, option can be turned off | A LAN attacker can set both and is accepted silently |
| Hijack of the intent window of a paired computer | Intent needs a paired key, short-lived, used once | Matched by source IP, so a host that can take that IP and reach the Quick Share port in that time is accepted |
| Flood of the trust port | Per-address rate limit, per-address and global caps, 10 s timeouts | Many addresses can fill the 32 slots and delay probes and pairing; transfers by consent still work |
| mDNS advert makes the core connect to an outside host | Probe only to local addresses | Any local address can be probed (a TCP connect to the trust port) |
| Theft of the identity key | File mode 0600, written through a private temp file | Not encrypted; another process of the same user can read it |
| Other process on the session bus calls the API (accept, trust, send) | None seen in the interface code: no check of the caller | Same trust boundary as the user's session |
| Huge metadata bloats the history | Caps on files and text per entry | Entry count is bounded only by age |
| Rich-text or HTML injection through a remote name or text | Shell escapes names and text in notifications, labels use plain text | See next section |
| Leak of Wi-Fi password through the history | Only the network name is kept | None known |
| Forged "trusted" display on a card | The trust label comes from the core's probe, not from the advert | Depends on the probe being correct |
| A nearby device uses the Bluetooth receiver (setting on) to read or cut a session | Only the device that opens the session with a valid connection request can write to it; one session at a time, new ones refused while one is live; a transfer still needs the user's consent and is never auto-accepted (ids `ble-N`) | A device can claim the slot in the instant between the phone's subscribe and its first write, and hold it while it keeps writing |
| A peer's upgrade offer points the computer at a hostile network | SSID and password are validated, the gateway must be a private address, the profile never takes the default route or DNS and is kept in memory | The computer joins a network chosen by the peer |
| The access point on the user's radio | Random 32-character key, profiles deleted afterwards and at start; NetworkManager's `dnsmasq` serves DHCP, so the daemon holds no extra privilege | Only the upgrade port is open to whoever joined (firewalld zone `nm-shared`); where firewalld cannot be changed the zone is `trusted` and every port is open |

## Display of remote names

Remote names are data chosen by the sender. The shell handles them in three places:

- `CoreBridge::plain()` replaces `<` and `>` with the single angle quotes U+2039 and U+203A, so a name cannot be read as rich text by a label. It is applied to device names, file lists, trusted and allowed device names, and pairing peer names and messages, whether they come from live signals or from the history.
- Notifications (`TransferWatcher`) pass device name, PIN and text preview through `toHtmlEscaped()`. The preview is shortened first.
- The text preview and the history text in the transfers page use `Text.PlainText`.

Not done: direction and zero-width characters are not removed from names shown in the UI. The core's guard covers file names only, so a device name can still contain them.

The "Open link" action appears only for received text that is a single `http://` or `https://` URL without spaces.

## Testing

Everything below runs in CI (`.github/workflows/ci.yml`) through `build/check.sh` in the Fedora 45 build image, with `QT_QPA_PLATFORM=offscreen`, together with `cargo fmt --check`, Clippy with `-D warnings` and `qmllint` (see [CONTRIBUTING.md](../CONTRIBUTING.md#checks)).

| Layer | Where | Covers |
| --- | --- | --- |
| Rust unit tests | `#[cfg(test)]` in `src/core/*.rs` | Name guard, rate limiter, ARP parsing and the trusted-sender rule, settings round trip and defaults, history, stale transfer cleanup, event merging, local-address filter, final states, code format. `trust.rs` tests run two real nodes over loopback: pairing, declined pairing, strangers refused, unnamed computer, unpair, per-address and global connection limits |
| Rust integration | `tests/core/dbus_api.rs` | Starts the real `kuickshared` binary and drives its D-Bus API as the shell does: default settings, persistence, reset, history remove, clear and expiry, legacy entries without an id, trusting an unknown transfer |
| Qt tests | `tests/shell/`, run by `ctest` | `TransferModel` (merging, final-state handling, history rows), `TransferFilterModel`, `DeviceModel`, Dolphin menu entry writing, Exec quoting |
| `rqs_lib` | `cargo test -p rqs_lib` (not part of the root run: it is not a workspace member) | The upgrade hand-over, plain and encrypted, the Bluetooth receiver and sender and the scanner, in memory and over loopback, with a fake `WifiLink` |
| Hardware | `#[ignore]` tests in `rqs_lib` (`hdl::ble_scan`, `hdl::ble_send::dial`) | Scan, dial and send with a real adapter and phone, run by hand outside the container; see [Wi-Fi upgrade](wifi-direct.md) |

The integration tests never touch the desktop's bus. Each test starts its own `dbus-daemon` on an abstract socket with a generated config (no service activation), and runs the core with `DBUS_SESSION_BUS_ADDRESS`, `HOME`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` pointing into a temp directory. `dbus-daemon` must be installed.

Not covered by automated tests:

- Real Quick Share transfers (with a phone or between two instances): the consent, auto-accept and name-guard paths in `forward_transfers` have no test that feeds them library events.
- Pairing between two real computers; it is tested over loopback only.
- `CoreBridge`, notifications, progress jobs and the QML UI, beyond `qmllint`.
- The Flatpak: only the headless smoke test above. Notifications, job progress, the file chooser and Dolphin reading the entry are unverified.
- The AppImage: `build/appimage/test-appimage.sh` is a headless smoke test in bare containers (Debian, Alpine, NixOS, and Ubuntu 22.04 under Xvfb without Wayland: the main window appears within 10 s). Following the desktop theme, tray, notifications, file dialogs, GPU rendering, Wayland and the first-launch dialog are unverified on a real desktop. The tarball installer: `build/test-tarball.sh`, also headless.

## Packaging

Three packages ship the same sources:

| | AppImage | Tarball | Flatpak |
|---|---|---|---|
| Libraries | Its own (Arch, Qt 6 and KF6, with ld.so and libc): runs on any distribution | System Qt 6 / KF6 | `org.kde.Platform` 6.11 |
| Build | `build/build-appimage.sh` | `build/build-tarball.sh` | `build/flatpak/build-flatpak.sh` |
| Distribution | GitHub release, with `.zsync` update information | GitHub release | Release asset (bundle) and a signed OSTree repository on GitHub Pages |
| Dolphin entry, autostart | Written by the app when you say so, with `$APPIMAGE` as `Exec` | Written by the installer and the app | Written by the app at runtime into the host's folders, with `flatpak run` as `Exec` |

```mermaid
flowchart LR
    H["build/build-host-payload.sh<br/>Fedora container: host-linked binaries,<br/>data, install.sh"] --> T["tarball"]
    R["Arch container: cargo + cmake<br/>quick-sharun bundle"] --> A["AppImage + .zsync"]
    T -->|"install.sh"| I["copy into ~/.local"]
```

### AppImage

Built the [Anylinux](https://github.com/pkgforge-dev/Anylinux-AppImages) way, in the Arch image `build/appimage/Containerfile.arch`, with `quick-sharun` (pinned by commit and SHA-256 in the Containerfile; it pins and checks `sharun`, `appimagetool` and the other helpers it downloads).

| Bundle | Content |
| --- | --- |
| Binaries | `kuickshare`, `kuickshared` |
| Runtime | The dynamic loader and libc, Qt 6 and KF6, the Qt and KF6 plugins |
| QML | Only the modules the UI imports (`DEPLOY_QML=1` would copy about 450 MB) |
| GPU drivers | None; the host's are used |
| Size | About 73 MiB |

- **Direct run.** The hook `build/appimage/anylinux.hook` (installed as `bin/00-kuickshare.hook`) lets a plain start run the bundled `kuickshare`. It sets `QT_QPA_PLATFORMTHEME=kde` (the bundle holds the Plasma platform theme and Breeze icons) so the app follows the host's theme, read from the host's `~/.config`.
- **Core.** `kuickshared` is started by the shell from its own folder. In the bundle it is a small script that runs the real binary (renamed `kuickshared-core`) through `sharun`, so it uses the bundled libc.
- **`$APPIMAGE`.** The AppImage runs from a temporary mount, so `Launcher::inAppImage()` (`src/shell/launcher.h`) makes the Dolphin, autostart and menu entries point at the `$APPIMAGE` file.
  - Detection: `APPIMAGE` names an existing file and the program is inside `APPDIR`. A tarball installation started from a terminal that inherited `APPIMAGE` is therefore not taken for the AppImage.
  - Nothing is written by itself: the first start asks once (`src/shell/integration.cpp`, `IntegrationDialog.qml`), and the Preferences toggles write entries on demand.
- **Updates.** `appimagetool` embeds `gh-releases-zsync|the-beper|KuickShare|latest|KuickShare-x86_64.AppImage.zsync` and writes the `.zsync` file, which the release publishes.
- **Cache.** The Podman volume `kuickshare-appimage` keeps the cargo registry; each build uses a fresh `TMPDIR` because `quick-sharun` does not re-check cached files.
- **Test.** `build/appimage/test-appimage.sh` runs the AppImage with `--appimage-extract-and-run` (no FUSE):
  - Where: bare Debian, Alpine and `nixos/nix` containers with only `dbus`, plus Ubuntu 22.04 with Xvfb and no Wayland (the main window appears within 10 s).
  - Checks: the core answers on D-Bus, no QML or plugin errors, only its own data is written, and a second launch hands its files over.

### Tarball

Built in three steps:

1. `build/build-host-payload.sh` builds `kuickshare` and `kuickshared` against the system's Qt 6 / KF6 in the Fedora image (`build/Containerfile`, started by `build/build-env.sh`, with the host's rustup and cargo mounted; the host needs no development packages).
2. The same script stages them in `dist/tmp/host` with the data files, the compiled translations (`msgfmt --check-format`, so a broken format string fails the build) and `build/appimage/install.sh`.
3. `build/build-tarball.sh` packs that folder with a top-level `install.sh` into a reproducible `.tar.gz`.

- **Host libraries.** Qt 6 and KF6 are not bundled in the payload ([THIRD_PARTY.md](../THIRD_PARTY.md) lists them). The installer checks with `ldd` for missing libraries and for the QML modules `org/kde/kirigami` and `org/kde/kirigamiaddons/formcard`, and prints the `dnf` or `pacman` command if something is missing.
- **Install.** Binaries are copied to a `.new` file and moved into place, so an update does not hit "text file busy". A running instance is stopped first (SIGTERM, then SIGKILL after 5 s) and restarted afterwards. The Dolphin entry is not reinstalled if `dolphin_integration` is `false` in the settings. `~/.local/bin/kuickshare` is linked only if it is missing or already ours. The translations go to `~/.local/share/locale/<lang>/LC_MESSAGES/kuickshare.mo`, where KDE looks for them.
- **Uninstall.** `~/.local/share/kuickshare/uninstall.sh` removes the files above. User data stays unless `--purge` is given.
- **Test.** `build/test-tarball.sh` installs, updates and uninstalls (`--purge`) in the Fedora image with a throw-away `HOME`.

### Flatpak

The manifest is [`build/flatpak/io.github.the_beper.KuickShare.yml`](../build/flatpak/io.github.the_beper.KuickShare.yml): one module for the Rust core (offline, with `cargo-sources.json` generated from `Cargo.lock` by `flatpak-cargo-generator`) and one CMake module for the shell.

```mermaid
flowchart LR
    A["build/flatpak/build-flatpak.sh"] -->|"runs in"| B["podman image, Fedora 45 (build/flatpak/Containerfile)"]
    B --> C["flatpak-builder: KDE 6.11 SDK, rust-stable"]
    C --> D["OSTree repo + dist/KuickShare-x86_64.flatpak"]
    D -->|"release.yml: sign, publish-repo.sh"| E["GitHub Pages: repo/, .flatpakrepo, .flatpakref"]
```

- **Build environment.** The container needs `--privileged` for bwrap. Runtimes and the build cache live in the podman volume `kuickshare-flatpak`, not on the host.
- **Sandbox holes.** The permissions are listed in the [usage guide](usage.md#flatpak). The shell must own `org.kde.kuickshare` (`KDBusService`; the app does not start without it). The tray item registers with its connection name, so it needs no name of its own (tested against a fake watcher).
- **Paths.** Inside the sandbox `XDG_*` point to `~/.var/app/<id>`, so `src/shell/launcher.h` finds the host's folders for the Dolphin and autostart entries in `/proc/self/mountinfo`.
  - Flatpak mounts the granted `kio/servicemenus` and `autostart` folders there at their host paths. If none is found, the default is `$HOME/.local/share` and `$HOME/.config`.
  - `Exec` becomes `flatpak run --file-forwarding <id> @@ %F @@`, which hands the files to the sandbox through the document portal.
  - With no installer, the shell writes the Dolphin entry on start when the setting is on and the file is missing.
  - A download folder is accepted only under a mount that is not one of the sandbox's private ones (`/tmp`, `/run`, `/usr`, ...), or under a portal document or `/run/media`; otherwise files written there would be lost with the sandbox. This check is covered by unit tests on sample mount tables, not yet in a real sandbox.
- **Repository.** `release.yml` builds with `--gpg-sign` (secret `FLATPAK_GPG_PRIVATE_KEY`), adds the commit to the repository fetched from the `gh-pages` branch, and `build/flatpak/publish-repo.sh` prunes it and writes `kuickshare.flatpakrepo` and `kuickshare.flatpakref` with the public key. The branch is replaced by a single commit each release so OSTree objects do not pile up in the git history.
- **Test.** `build/flatpak/test-flatpak.sh` installs the bundle in a container with offscreen Qt, a private session bus, non-default `XDG_*` folders, a fake `StatusNotifierWatcher` and the document portal. It checks that:
  - the core answers on D-Bus;
  - the tray item registers;
  - the Dolphin entry lands in the host folder;
  - a file is forwarded to the sandbox;
  - a second launch hands over to the running instance.

  Notifications, job progress, the file chooser and Dolphin itself need a real Plasma session and are untested.
