# KuickShare

Quick Share client for KDE Plasma 6: Rust daemon `kuickshared` (`src/core`, protocol from the vendored `third_party/rqs_lib`) plus a Qt 6 / Kirigami shell `kuickshare` (`src/shell`, C++ and QML). Layout, build and checks are in [CONTRIBUTING.md](../CONTRIBUTING.md); do not copy them here. Architecture: [docs/architecture.md](../docs/architecture.md). D-Bus API: [docs/dbus-api.md](../docs/dbus-api.md).

## Commands
- Everything that needs Qt, KF6, ECM or `dbus-daemon` runs through `build/build-env.sh <command>` (podman). A bare `cmake` on the host fails with a missing ECM.
- Full check: the `/check` skill, which runs `build/build-env.sh build/check.sh` (the list of checks lives in that script, CI runs it too) plus the `po.py` catalog status. `build/Containerfile` is also CI's image (ghcr, `images.yml`): after changing it, rebuild the local one with `podman build -t localhost/kuickshare-build:fedora45 -f build/Containerfile build`.
- Version bump: only with `build/release/version-bump.sh X.Y.Z NOTES` (it checks the version and writes `Cargo.toml`, `Cargo.lock`, `src/shell/CMakeLists.txt` and the metainfo `<release>`), never by hand; see Making a release in CONTRIBUTING.md.
- Dev build: `build/build-env.sh cargo build` and `build/build-env.sh cmake --build dist/tmp/cmake-dev`. `dist/` and `target/` are not in git. `scripts/testing/run.sh` starts the build with settings and history in RAM.

## Conventions that are not obvious
- `third_party/rqs_lib` is a modified copy of rquickshare, not a workspace member. List every change to it in the table in [THIRD_PARTY.md](../THIRD_PARTY.md).
- Docs mirror the code and are updated in the same change. [docs/wifi-direct.md](../docs/wifi-direct.md) marks causes and claims taken from logs or code, not reproduced, with *(inferred)*; keep that marker until a run or a test shows the cause. Never say a hardware path works because the tests pass: BLE, Wi-Fi and NetworkManager have no automated test with a real device.
- Translations: strings are `i18n()` calls (in QML `KI18n.i18n()`, with `import org.kde.ki18n`) in the files listed in `po/POTFILES.in` (add new files there). `po/kuickshare.pot` is generated and a hook blocks editing it. After changing strings run `/update-translations`: it regenerates with `po/extract.sh` and the `translator` agent (haiku) fills `it`, `nl`, `tr`; `en` repeats the msgid. `nl` and `tr` are not natively reviewed.
- Message boxes use `MessageBox.qml`, never `Kirigami.InlineMessage` directly: it is visible by default and keeps the icon (info, warning, error) vertically centered. Box texts never end with a period. Warning boxes use `Layout.fillWidth` and `Layout.topMargin: Kirigami.Units.largeSpacing`, as in `SettingsPage.qml`.
- Deliberate exceptions to the KDE HIG: the main window has a fixed size (26 grid units wide, not resizable or maximizable) and its pages scroll; the tray icon is on by default (opt-out), because receiving in the background is the main use; the settings use kirigami-addons `FormCard`, not `Kirigami.FormLayout`.
- The hotspot subnet `192.168.49.0/24` is written in two places: `GATEWAY` in `src/core/link.rs` and the mDNS exclusion in `third_party/rqs_lib/src/hdl/mdns.rs`. Change both.
- Outgoing transfers are keyed by the device id (`ble:XXXX` or `ip:port`), which repeats: `TransferModel` treats a non-final event on a finished id as a new row. A send adds a "Starting" row at once (`TransferModel::beginPending`).
- BLE timings are tied together: scan window 12 s for a send, GATT dial 22 s, watchdog `BLE_START_TIMEOUT` 40 s in `src/core/app.rs`, shell timer 45 s in `corebridge.cpp`; at stop the library's wind-down 0.4 s (`STOP_WIND_DOWN` in rqs_lib) < the engine's stop 0.5 s (`SHUTDOWN_STOP`) + drain 0.2 s, all within `SHUTDOWN_ENGINE_BOUND` 0.8 s (the wait for the Core lock included), then Wi-Fi cleanup 2 s (total 2.8 s), within the shell's 3 s SIGTERM wait; a test asserts these. Change one, check the others.
- Never hold the Core lock across slow I/O or D-Bus awaits.

## Hand tests with the phones
See the `/phone-test` skill. Time zones differ between log files (daemon UTC, system and btmon local). Only a Pixel 7 and a Galaxy A55 were ever tested.

## Working with the user
- Run `rqs-reviewer` besides `code-reviewer` when `third_party/rqs_lib`, `link.rs` or `app.rs` changed.
- Run `kde-hig-reviewer` besides `code-reviewer` when `src/shell/qml`, user-visible strings, notifications or the desktop/metainfo files changed.
- Project agents in `.claude/agents/`: `translator` (haiku, catalogs), `phone-log-analyzer` (a `dist/logs` session), `rqs-reviewer` (transfer code).
- When an agent edits code, tell it not to build; build and test once at the end.
