# Third-party software

KuickShare is GPL-3.0-or-later (see [LICENSE](LICENSE)). It builds on the projects below.

## Origin

| Project | License | Role |
| --- | --- | --- |
| [Packet](https://github.com/nozwock/packet) | GPL-3.0-or-later | KuickShare is derived from it |
| [rquickshare / `rqs_lib`](https://github.com/nozwock/rquickshare) | GPL-3.0 | Quick Share protocol implementation, vendored as a path dependency in [`third_party/rqs_lib`](third_party/rqs_lib) (see below) |

### Changes to `rqs_lib`

`third_party/rqs_lib` is a modified copy of rquickshare `core_lib` at rev `c9ac93f5584a4265e8da679ad689df5be1e80c6b`. Changes against that revision:

| Change | Files |
| --- | --- |
| WIFI_HOTSPOT and WIFI_DIRECT bandwidth upgrade in both directions, plain frames when the host disables encryption (new) | `src/upgrade.rs`, `src/upgrade/tests.rs`; handlers in `src/hdl/inbound.rs`, `src/hdl/outbound.rs`; `src/lib.rs` (`set_wifi_link`) |
| Receive session over a non-TCP stream, cancel-safe length prefix, keep-alive tolerance in the handshake | `src/hdl/migratable.rs` (new), `src/hdl/inbound.rs`, `src/hdl/outbound.rs`, `src/utils.rs`, `src/manager.rs` |
| Bluetooth receiver (new) | `src/hdl/ble_receiver/`, `src/hdl/mod.rs`, `src/lib.rs` |
| Bluetooth sender: windowed scan, L2CAP dial and bridge, BLE endpoints in `EndpointInfo` and `SendInfo` (new) | `src/hdl/ble_scan.rs`, `src/hdl/ble_send/`, `src/lib.rs`, `src/manager.rs`, `src/hdl/mdns_discovery.rs` |
| BLE scanner removed (`src/hdl/ble.rs`, `btleplug`); periodic mDNS re-announce instead | `src/hdl/mdns.rs`, `src/lib.rs`, `Cargo.toml` |
| The `MDnsServer` task logs its error instead of dropping it | `src/lib.rs` |
| Four fixes:<br>- mDNS ignores the hotspot subnet<br>- a send reads the phone's cancel or disconnection while it writes<br>- a phone known to advertise no PSM is dialled over GATT at once<br>- a pending BLE connect is cancelled when a dial ends | `src/hdl/mdns.rs`, `src/hdl/mdns_discovery.rs`, `src/hdl/outbound.rs`, `src/utils.rs`, `src/hdl/ble_scan.rs`, `src/hdl/ble_send/` |
| Received file names: only the last path component is kept (a peer cannot write outside the download directory), and a missing or generic (`.tmp`, `.bin`) extension is taken from the `mime_type` when unambiguous | `src/utils.rs` (`received_file_name`), `src/hdl/inbound.rs` |
| Inbound files never overwrite one another: names are made unique across the files of one offer, and the file is created with `create_new` at accept time, picking the next free name if another transfer took it meanwhile (`saved_paths` is not updated in that rare case) | `src/hdl/inbound.rs` (`free_path`, `accept_transfer`) |
| The receiver advertisement asks for the 1M secondary PHY (extended PDU), as its service data exceeds a legacy PDU (the failure without it is taken from upstream, *(inferred)*, not reproduced) | `src/hdl/ble_receiver/advertiser.rs` |
| `TransferMetadata::saved_paths`: where an inbound file is saved | `src/hdl/info.rs`, `src/hdl/inbound.rs`, `src/hdl/outbound.rs` |
| `TransferPayload` has a hand-written `Debug` that redacts a Wi-Fi password, and the errors of the Wi-Fi payload parser no longer quote the raw buffer (it holds the password) | `src/hdl/info.rs`, `src/hdl/inbound.rs` |
| Per-chunk logs lowered to `trace` | `src/hdl/inbound.rs`, `src/hdl/outbound.rs` |
| `sys_metrics` (AGPL-3.0) replaced by reading `/proc/sys/kernel/hostname`; `get_if_addrs` replaced by `if-addrs` | `src/utils.rs`, `src/lib.rs`, `Cargo.toml` |
| `mdns-sd` from crates.io (0.21), and `if-addrs` 0.15 to match it, instead of the `unsolicited` branch of a fork: the periodic announcement is a second `register` call, the advertised addresses are IPv4 only through `disable_interface(IfKind::IPv6)` on the advertising daemon, probing is off (`set_requires_probe(false)`), and the host name carries the `.local.` suffix that 0.21 requires | `Cargo.toml`, `src/hdl/mdns.rs` |
| `license = "GPL-3.0-only"` added to the manifest (upstream declares none, cargo-deny warned) | `Cargo.toml` |
| Formatting only (and an unused test import) | `src/hdl/mdns_discovery.rs`, `src/hdl/blea.rs` (comment), `src/hdl/outbound.rs`, `src/hdl/ble_scan.rs`, `src/hdl/ble_send/gatt_dial.rs`, `src/manager.rs` |

## System software and bundled libraries

These are used from the host by the tarball installer (it checks that they are present) and by the Flatpak's runtime-less parts. The Flatpak uses the KDE runtime instead.

The **AppImage ships them**, so their licenses apply to that file too:

- It bundles Qt 6, KDE Frameworks 6, Kirigami, the Plasma platform theme, the Breeze icons, glibc and the other libraries the two binaries load.
- They are built from Arch Linux packages by `quick-sharun`, see [`build/build-appimage.sh`](build/build-appimage.sh).
- Most are LGPL, which allows relinking: the libraries are separate files inside the AppImage and can be replaced after `--appimage-extract`.
- The full list of bundled files is `find squashfs-root` after extracting.

| Software | License |
| --- | --- |
| Qt 6 (Base, Declarative, SVG) | LGPL-3.0 (modules also available under GPL) |
| KDE Frameworks 6 (KI18n, KConfig, KCoreAddons, KDBusAddons, KIO, KIconThemes, KNotifications, KStatusNotifierItem) | mostly LGPL-2.0-or-later / LGPL-2.1-or-later; see each framework |
| Kirigami | LGPL-2.0-or-later |
| kirigami-addons | LGPL-2.0-or-later (some parts GPL/MIT; unverified per file) |
| Breeze icon theme | LGPL-3.0-or-later |

Icons are referenced by theme name and resolved from the system Breeze theme at run time; none are copied into this repository except KuickShare's own app icon.

## Rust crates

The tarball (the host-library package) contains only the two binaries `kuickshare` and `kuickshared` (see [`build/build-host-payload.sh`](build/build-host-payload.sh)). All crates below are statically linked into `kuickshared`.

The list is generated from `cargo metadata` (`Cargo.lock`), restricted to normal (non-dev, non-build-only) dependencies reachable from `kuickshare-core`, and covers all target platforms, so a few entries may not be compiled on Linux. Licenses are the SPDX expressions declared by each crate.

### Direct dependencies

| Crate | Version | License |
| --- | --- | --- |
| anyhow | 1.0.104 | MIT OR Apache-2.0 |
| dirs | 7.0.0 | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 |
| hex | 0.4.3 | MIT OR Apache-2.0 |
| rand | 0.10.3 | MIT OR Apache-2.0 |
| rqs_lib | 0.11.5 | GPL-3.0-only |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.154 | MIT OR Apache-2.0 |
| sha2 | 0.11.1 | MIT OR Apache-2.0 |
| snow | 0.10.0 | Apache-2.0 OR MIT |
| tokio | 1.54.0 | MIT |
| tracing | 0.1.44 | MIT |
| tracing-subscriber | 0.3.23 | MIT |
| zbus | 6.0.0 | MIT |

### Transitive dependencies

<details>
<summary>223 crates</summary>

| Crate | Version | License |
| --- | --- | --- |
| aead | 0.5.2 | MIT OR Apache-2.0 |
| aes | 0.8.4 | MIT OR Apache-2.0 |
| aes-gcm | 0.10.3 | Apache-2.0 OR MIT |
| aho-corasick | 1.1.5 | Unlicense OR MIT |
| async-trait | 0.1.92 | MIT OR Apache-2.0 |
| base16ct | 0.2.0 | Apache-2.0 OR MIT |
| base64 | 0.22.1 | MIT OR Apache-2.0 |
| base64ct | 1.8.3 | Apache-2.0 OR MIT |
| bitflags | 1.3.2 | MIT/Apache-2.0 |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| blake2 | 0.10.6 | MIT OR Apache-2.0 |
| block-buffer | 0.10.4 | MIT OR Apache-2.0 |
| block-buffer | 0.12.1 | MIT OR Apache-2.0 |
| bluer | 0.17.4 | BSD-2-Clause |
| bumpalo | 3.20.3 | MIT OR Apache-2.0 |
| bytes | 1.12.1 | MIT |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |
| chacha20 | 0.9.1 | Apache-2.0 OR MIT |
| chacha20poly1305 | 0.10.1 | Apache-2.0 OR MIT |
| cipher | 0.4.4 | MIT OR Apache-2.0 |
| concurrent-queue | 2.5.0 | Apache-2.0 OR MIT |
| const-oid | 0.10.2 | Apache-2.0 OR MIT |
| const-oid | 0.9.6 | Apache-2.0 OR MIT |
| cpufeatures | 0.2.17 | MIT OR Apache-2.0 |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 |
| crypto-bigint | 0.5.5 | Apache-2.0 OR MIT |
| crypto-common | 0.1.7 | MIT OR Apache-2.0 |
| crypto-common | 0.2.2 | MIT OR Apache-2.0 |
| ctr | 0.9.2 | MIT OR Apache-2.0 |
| curve25519-dalek | 4.1.3 | BSD-3-Clause |
| curve25519-dalek-derive | 0.1.1 | MIT/Apache-2.0 |
| custom_debug | 0.6.2 | Apache-2.0 OR MIT |
| custom_debug_derive | 0.6.2 | Apache-2.0 OR MIT |
| darling | 0.20.11 | MIT |
| darling_core | 0.20.11 | MIT |
| darling_macro | 0.20.11 | MIT |
| dbus | 0.9.12 | Apache-2.0/MIT |
| dbus-crossroads | 0.5.3 | Apache-2.0/MIT |
| dbus-tokio | 0.7.6 | Apache-2.0/MIT |
| der | 0.7.10 | Apache-2.0 OR MIT |
| digest | 0.10.7 | MIT OR Apache-2.0 |
| digest | 0.11.3 | MIT OR Apache-2.0 |
| directories | 6.0.0 | MIT OR Apache-2.0 |
| dirs-sys | 0.5.0 | MIT OR Apache-2.0 |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 |
| ecdsa | 0.16.9 | Apache-2.0 OR MIT |
| either | 1.19.0 | MIT OR Apache-2.0 |
| elliptic-curve | 0.13.8 | Apache-2.0 OR MIT |
| endi | 1.1.1 | MIT |
| errno | 0.3.14 | MIT OR Apache-2.0 |
| fastrand | 2.5.0 | Apache-2.0 OR MIT |
| ff | 0.13.1 | MIT/Apache-2.0 |
| fiat-crypto | 0.2.9 | MIT OR Apache-2.0 OR BSD-1-Clause |
| flume | 0.11.1 | Apache-2.0/MIT |
| fnv | 1.0.7 | Apache-2.0 / MIT |
| futures | 0.3.34 | MIT OR Apache-2.0 |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 |
| futures-core | 0.3.34 | MIT OR Apache-2.0 |
| futures-executor | 0.3.34 | MIT OR Apache-2.0 |
| futures-io | 0.3.34 | MIT OR Apache-2.0 |
| futures-macro | 0.3.34 | MIT OR Apache-2.0 |
| futures-sink | 0.3.34 | MIT OR Apache-2.0 |
| futures-task | 0.3.34 | MIT OR Apache-2.0 |
| generic-array | 0.14.7 | MIT |
| getrandom | 0.2.17 | MIT OR Apache-2.0 |
| getrandom | 0.3.4 | MIT OR Apache-2.0 |
| getrandom | 0.4.3 | MIT OR Apache-2.0 |
| ghash | 0.5.1 | Apache-2.0 OR MIT |
| group | 0.13.0 | MIT/Apache-2.0 |
| heck | 0.5.0 | MIT OR Apache-2.0 |
| hkdf | 0.12.4 | MIT OR Apache-2.0 |
| hmac | 0.12.1 | MIT OR Apache-2.0 |
| hybrid-array | 0.4.15 | MIT OR Apache-2.0 |
| ident_case | 1.0.1 | MIT/Apache-2.0 |
| if-addrs | 0.10.2 | MIT OR BSD-3-Clause |
| inout | 0.1.4 | MIT OR Apache-2.0 |
| itertools | 0.14.0 | MIT OR Apache-2.0 |
| itoa | 1.0.18 | MIT OR Apache-2.0 |
| js-sys | 0.3.106 | MIT OR Apache-2.0 |
| lazy_static | 1.5.1 | MIT OR Apache-2.0 |
| libaes | 0.7.0 | Apache-2.0 OR MIT |
| libc | 0.2.191 | MIT OR Apache-2.0 |
| libdbus-sys | 0.2.7 | Apache-2.0/MIT |
| libredox | 0.1.25 | MIT |
| linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| lock_api | 0.4.14 | MIT OR Apache-2.0 |
| log | 0.4.34 | MIT OR Apache-2.0 |
| macaddr | 1.0.1 | Apache-2.0 OR MIT |
| matchers | 0.2.0 | MIT |
| mdns-sd | 0.21.6 | Apache-2.0 OR MIT |
| memchr | 2.8.3 | Unlicense OR MIT |
| memoffset | 0.9.1 | MIT |
| mime | 0.3.17 | MIT OR Apache-2.0 |
| mime_guess | 2.0.5 | MIT |
| mio | 1.2.4 | MIT |
| nix | 0.29.0 | MIT |
| nu-ansi-term | 0.50.3 | MIT |
| num-bigint | 0.4.8 | MIT OR Apache-2.0 |
| num-derive | 0.4.2 | MIT OR Apache-2.0 |
| num-integer | 0.1.47 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| opaque-debug | 0.3.1 | MIT OR Apache-2.0 |
| option-ext | 0.2.0 | MPL-2.0 |
| ordered-stream | 0.2.0 | MIT OR Apache-2.0 |
| p256 | 0.13.2 | Apache-2.0 OR MIT |
| pem-rfc7468 | 0.7.0 | Apache-2.0 OR MIT |
| pin-project | 1.1.13 | Apache-2.0 OR MIT |
| pin-project-internal | 1.1.13 | Apache-2.0 OR MIT |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT |
| pkcs8 | 0.10.2 | Apache-2.0 OR MIT |
| polling | 2.8.0 | Apache-2.0 OR MIT |
| poly1305 | 0.8.0 | Apache-2.0 OR MIT |
| polyval | 0.6.2 | Apache-2.0 OR MIT |
| ppv-lite86 | 0.2.21 | MIT OR Apache-2.0 |
| primeorder | 0.13.6 | Apache-2.0 OR MIT |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| prost | 0.13.5 | Apache-2.0 |
| prost-derive | 0.13.5 | Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| r-efi | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| rand_chacha | 0.9.0 | MIT OR Apache-2.0 |
| rand_core | 0.6.4 | MIT OR Apache-2.0 |
| rand_core | 0.9.5 | MIT OR Apache-2.0 |
| redox_users | 0.5.3 | MIT |
| regex-automata | 0.4.18 | MIT OR Apache-2.0 |
| regex-syntax | 0.8.11 | MIT OR Apache-2.0 |
| rfc6979 | 0.4.0 | Apache-2.0 OR MIT |
| ring | 0.17.14 | Apache-2.0 AND ISC |
| rustix | 1.1.5 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| rustversion | 1.0.23 | MIT OR Apache-2.0 |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 |
| sec1 | 0.7.3 | Apache-2.0 OR MIT |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_repr | 0.1.21 | MIT OR Apache-2.0 |
| sha2 | 0.10.9 | MIT OR Apache-2.0 |
| sharded-slab | 0.1.7 | MIT |
| signal-hook-registry | 1.4.8 | MIT OR Apache-2.0 |
| signature | 2.2.0 | Apache-2.0 OR MIT |
| slab | 0.4.12 | MIT |
| smallvec | 1.16.3 | MIT OR Apache-2.0 |
| socket2 | 0.4.10 | MIT OR Apache-2.0 |
| socket2 | 0.6.5 | MIT OR Apache-2.0 |
| spin | 0.9.9 | MIT |
| spki | 0.7.3 | Apache-2.0 OR MIT |
| strsim | 0.11.1 | MIT |
| strum | 0.26.3 | MIT |
| strum_macros | 0.26.4 | MIT |
| subtle | 2.6.1 | BSD-3-Clause |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.9 | MIT OR Apache-2.0 |
| synstructure | 0.13.2 | MIT |
| tempfile | 3.27.0 | MIT OR Apache-2.0 |
| thiserror | 2.0.21 | MIT OR Apache-2.0 |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 |
| thread_local | 1.1.10 | MIT OR Apache-2.0 |
| tokio-macros | 2.7.2 | MIT |
| tokio-stream | 0.1.19 | MIT |
| tokio-util | 0.7.20 | MIT |
| tracing-attributes | 0.1.31 | MIT |
| tracing-core | 0.1.36 | MIT |
| tracing-log | 0.2.0 | MIT |
| typenum | 1.20.1 | MIT OR Apache-2.0 |
| uds_windows | 1.2.1 | MIT |
| unicase | 2.10.0 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| universal-hash | 0.5.1 | MIT OR Apache-2.0 |
| untrusted | 0.9.0 | ISC |
| uuid | 1.28.0 | Apache-2.0 OR MIT |
| valuable | 0.1.1 | MIT |
| wasi | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasip2 | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasm-bindgen | 0.2.129 | MIT OR Apache-2.0 |
| wasm-bindgen-macro | 0.2.129 | MIT OR Apache-2.0 |
| wasm-bindgen-macro-support | 0.2.129 | MIT OR Apache-2.0 |
| wasm-bindgen-shared | 0.2.129 | MIT OR Apache-2.0 |
| winapi-i686-pc-windows-gnu | 0.4.0 | MIT/Apache-2.0 |
| winapi-x86_64-pc-windows-gnu | 0.4.0 | MIT/Apache-2.0 |
| windows-link | 0.2.1 | MIT OR Apache-2.0 |
| windows-sys | 0.48.0 | MIT OR Apache-2.0 |
| windows-sys | 0.52.0 | MIT OR Apache-2.0 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 |
| windows-targets | 0.48.5 | MIT OR Apache-2.0 |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 |
| windows_aarch64_gnullvm | 0.48.5 | MIT OR Apache-2.0 |
| windows_aarch64_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_aarch64_msvc | 0.48.5 | MIT OR Apache-2.0 |
| windows_aarch64_msvc | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_gnu | 0.48.5 | MIT OR Apache-2.0 |
| windows_i686_gnu | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_msvc | 0.48.5 | MIT OR Apache-2.0 |
| windows_i686_msvc | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_gnu | 0.48.5 | MIT OR Apache-2.0 |
| windows_x86_64_gnu | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_gnullvm | 0.48.5 | MIT OR Apache-2.0 |
| windows_x86_64_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.48.5 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 |
| winnow | 1.0.4 | MIT |
| wit-bindgen | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| zbus_macros | 6.0.0 | MIT |
| zbus_utils | 6.0.0 | MIT |
| zcheapstr | 1.1.0 | MIT |
| zerocopy | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| zerocopy-derive | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| zeroize | 1.9.1 | Apache-2.0 OR MIT |
| zmij | 1.0.23 | MIT |
| zruntime | 0.2.0 | MIT |

</details>
