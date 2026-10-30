# Contributing

KuickShare targets KDE Plasma 6: the AppImage and the Flatpak any distribution, the tarball Fedora 45 and distributions with the same libraries. Issues and pull requests are welcome; for anything larger than a small fix, open an issue first to agree on the approach.

## Repository layout

| Path | Content |
|---|---|
| `Cargo.toml`, `Cargo.lock` | Rust project (root) |
| `src/core/` | Daemon `kuickshared`: Quick Share protocol (uses [`third_party/rqs_lib`](THIRD_PARTY.md#changes-to-rqs_lib), a modified copy of rquickshare), settings, D-Bus API. Its unit tests sit next to the code |
| `src/shell/` | C++ and QML app `kuickshare` (Qt 6, KF6, Kirigami), with its CMake project |
| `tests/core/` | Rust integration tests; they start the daemon on a private D-Bus |
| `tests/shell/` | Qt tests: transfer model, Dolphin menu entry, `Exec` quoting, Flatpak and AppImage launcher paths, first-launch integration |
| `data/` | Files installed on the host: `.desktop`, metainfo, icons, notification definitions, Dolphin service menu |
| `build/` | Build container (`build-env.sh`, `Containerfile`), all checks (`check.sh`), AppImage (`build-appimage.sh`, `appimage/`: Arch image, hook, test), tarball with its installer (`build-host-payload.sh`, `build-tarball.sh`, `appimage/install.sh`, `test-tarball.sh`) and Flatpak (`flatpak/`: manifest, build, test and repository scripts) |
| `scripts/` | Developer tools: hand tests with a RAM sandbox (`testing/`) and documentation screenshots (`screenshots/`) |
| `po/` | Translation catalogs |
| `docs/` | User and design documentation |
| `dist/` | Generated, not in git: the AppImage and its `.zsync`, the tarball, the Flatpak bundle and repository, and `tmp/` (CMake builds, staging) |

## Build

Dependencies live in a podman image defined by [`build/Containerfile`](build/Containerfile), so nothing is installed on the host. `build/build-env.sh` runs a command inside it (building the image on first use). Rust comes from the host's rustup (stable, edition 2024).

```sh
build/build-env.sh cargo build
build/build-env.sh sh -c 'cmake -S src/shell -B dist/tmp/cmake-dev -G Ninja && cmake --build dist/tmp/cmake-dev'
build/build-appimage.sh    # dist/KuickShare-x86_64.AppImage and .zsync (Fedora and Arch images; first run is long)
build/appimage/test-appimage.sh dist/KuickShare-x86_64.AppImage
build/build-tarball.sh && build/test-tarball.sh dist/KuickShare-x86_64.tar.gz
build/flatpak/build-flatpak.sh && build/flatpak/test-flatpak.sh dist/KuickShare-x86_64.flatpak
```

The Flatpak build runs in its own container image (`build/flatpak/Containerfile`), started by the scripts with rootless podman (they refuse a rootful one) and `--security-opt unmask=ALL --cap-add SYS_ADMIN --cap-add NET_ADMIN` (bwrap mounts its own `/proc` and creates namespaces; the capabilities hold inside the container's user namespace only); there is no `--privileged`. CI and the release job run the same scripts on the plain runner, not in a job container (that would be a rootful, privileged one). The KDE runtime, about 2 GB, and the build cache are kept in the podman volume `kuickshare-flatpak`; `podman volume rm kuickshare-flatpak` removes them. Nothing is installed on the host.

## Run from the build tree

```sh
dist/tmp/cmake-dev/kuickshare [files...]
```

`--background` starts hidden in the tray and `--page send|transfers|settings|help` opens a page (see `--help`). The binary starts the daemon it finds next to it (`dist/tmp/cmake-dev/kuickshared` links to `target/debug/kuickshared`).

The app also needs a few files from `data/` on the host:

| File | Destination |
|---|---|
| `kuickshare.notifyrc` | `~/.local/share/knotifications6/` |
| icons | `~/.local/share/icons/hicolor/` |
| `io.github.the_beper.KuickShare.desktop` | `~/.local/share/applications/`, with `Exec` pointing at the built binary |
| `kuickshare-send.desktop` (executable) | `~/.local/share/kio/servicemenus/`, with `Exec` pointing at the built binary |

## Checks

One script runs every automated check, locally and in CI: [`build/check.sh`](build/check.sh). Run it before opening a pull request.

```sh
build/build-env.sh build/check.sh
```

| Step | Command |
|---|---|
| Format | `cargo fmt --check`, also in `third_party/rqs_lib` |
| Lint | `cargo clippy --locked --all-targets --all-features -- -D warnings` |
| Rust tests | `cargo test --locked --all-features` (needs `dbus-daemon`), and `cargo test -p rqs_lib` for the vendored library, which is not a workspace member |
| Shell | CMake build with `-DBUILD_TESTING=ON` into `dist/tmp/cmake-dev` (the Qt tests are built only then; the AppImage, tarball and Flatpak builds pass `-DBUILD_TESTING=OFF`), `ctest`, `qmllint-qt6` |
| Data | `xmllint` on the metainfo, `desktop-file-validate` on the app's `.desktop`, a structural check of the Dolphin service menu `kuickshare-send.desktop` (`desktop-file-validate` rejects its `Type=Service`), `msgfmt -c` on every catalog |
| Flatpak pins | the SDK and runtime pinned in `build/flatpak/build-flatpak.sh` are the runtime version of the manifest, and every pinned commit is a full id; CI's tarball job uses the Rust of `release.yml`; `release.yml` names each of the three build images by `@sha256:` digest, never by tag alone |

CI ([`ci.yml`](.github/workflows/ci.yml)) runs the same script in the image built from [`build/Containerfile`](build/Containerfile) and published to `ghcr.io` by [`images.yml`](.github/workflows/images.yml) (which also publishes the Flatpak and the AppImage Arch images; the AppImage and tarball jobs run in parallel and cache the Rust build); a pull request that changes a Containerfile is checked with the previous image until it is merged. A separate job runs `cargo deny check` ([`deny.toml`](deny.toml): advisories, licenses, sources), and a `lint` job runs `shellcheck` on the `*.sh` and `*.hook` files of `build/`, `scripts/` and `po/` and `actionlint` on the workflows. Dependabot proposes Cargo and action updates weekly, waits 7 days after a release before proposing it, keeps at most 3 action and 5 Cargo pull requests open, and groups only patch updates of Cargo (a minor or major update is its own pull request).

CI runs on pull requests, on pushes to `main`, on manual runs and every Monday on `main` (a scheduled run is only `cargo deny`, for new advisories); a tag does not run it again. The `changes` job ([`build/ci/changes.sh`](build/ci/changes.sh)) skips what a pull request cannot affect: nothing but documentation runs only `lint`, and the AppImage, tarball and Flatpak jobs run only when the build scripts, workflows, `Cargo.*`, the shell's `CMakeLists.txt` or QML, `data/` or `po/LINGUAS` change; pushes to `main` and manual runs run everything, and a scheduled run runs only `deny` (`lint` and the rest are skipped on purpose). The final `required` job is green only when every job that had to run succeeded, so it is the one check to require.

### Hand tests with a phone (Wi-Fi upgrade and Bluetooth)

The feature needs a real adapter and phone and cannot run in CI. Build with `build/build-env.sh cargo build` and the GUI as above, then run the programs on the host:

- Receiving needs the `dnsmasq` package on the host: NetworkManager runs it to give the phone an address on the hotspot. Neither direction needs privileges.
- Write the daemon log to a file: `RUST_LOG=info,rqs_lib::hdl::ble_receiver=debug,rqs_lib::upgrade=debug target/debug/kuickshared > kuickshared.log 2>&1` (it logs to stdout), and capture Bluetooth with `sudo btmon -w capture.btsnoop`.
- The phone's side is in `adb logcat -s NearbyConnections NearbyMediums NearbySharing WifiP2pService` (USB debugging on).
- Individual steps have `#[ignore]` tests. Run `cargo test -p rqs_lib --no-run` (inside the container; the printed path is the test binary) and execute that binary on the host with `--ignored --nocapture <name>`.
- Quick Share must show the Receive screen on the phone. A Samsung phone may advertise only in short bursts.
- Scripts in `scripts/testing/` (settings, history and received files of the daemon and the shell stay in RAM under `/dev/shm`, deleted when the test ends; the host is not touched, but Bluetooth, Wi-Fi and NetworkManager are real):

| Script | Use |
|---|---|
| `run.sh` | Quick run after a rebuild: daemon and shell, no sudo, no captures; starts with a synthetic transfer history (`demo-history.py`, `NO_DEMO=1` for an empty one) |
| `test-wifi.sh` | Desktop hand test with `btmon`, journal, `adb logcat` and NAT captures in `dist/logs/<timestamp>/` |
| `test-laptop.sh` | Same test on a laptop over ssh; copy it next to the binaries, state goes to `./state` |
| `sandbox.sh` | The RAM home, sourced by the others |

Quit any installed KuickShare first: the scripts need the session bus name.

| Test | What it does |
|---|---|
| `probe_nearby_phones` | Runs one Bluetooth LE scan window and prints what it sees |
| `send_small_file_to_phone` | Sends a small file over L2CAP (or GATT with `KS_TRANSPORT=gatt`) |
| `observe_upgrade_offer` | Sends a larger file and prints the phone's Wi-Fi upgrade offer, then declines it and stays on Bluetooth |
| `send_through_rqs_api` | Same through the public `RQS` API, without the app |
| `gatt_connection_confirm` | Checks the GATT connection to the phone |

| Variable | Used by | Meaning |
|---|---|---|
| `KS_ENDPOINT` | all but `probe_nearby_phones` | 4 character endpoint id of the phone to use |
| `KS_TRANSPORT` | `send_small_file_to_phone` | `gatt` sends over GATT even if the phone advertises a PSM |
| `KS_FILE` | `send_small_file_to_phone` | File to send instead of 8 KB of text |
| `KS_SIZE_KB` | `observe_upgrade_offer`, `send_through_rqs_api` | Size of the temporary file (default 3072) |

## Code style

- Rust: `rustfmt` defaults, Clippy clean, `Result` with context instead of `unwrap()` outside tests.
- C++ and QML: match the surrounding code.
- Comments, docs and commit messages in English.
- Keep changes small and focused. Add a test for non-trivial logic.
- Update the docs in the same change as the code.

## Screenshots

The images in `docs/screenshots/` come from the real shell running against a fake core, so they show made-up devices and never the data of the machine that takes them. [`scripts/screenshots/`](scripts/screenshots/) has the scripts:

| Script | Purpose |
|---|---|
| `all.sh` | Retakes every screenshot that can be scripted |
| `shoot.sh <name> [options]` | Captures one window as `docs/screenshots/<name>.png` (`OUT` changes the folder, `WAIT` the seconds before the capture) |
| `demo.sh [options]` | Opens the shell on its own private D-Bus bus, with settings in a temporary folder |
| `fake_core.py` | The made-up devices, transfers and history; edit it to change what the screenshots show |

They run on the host, not in the container: they need `spectacle`, `dbus-daemon`, `gdbus` and Python with PyGObject, and the shell built in `dist/tmp/cmake-dev` (see [Build](#build)). The windows are captured as the app opens them, at its fixed size, using the colours and icon theme of the desktop: switch to the light theme and close other windows first. `pair.png` needs a click and is taken by hand (Preferences, Pair a device).

## Translations

Texts in the shell are marked with `i18n()` (in QML `KI18n.i18n()` / `KI18n.i18np()`, with `import org.kde.ki18n`). After adding or changing any, regenerate the catalogs:

```sh
build/build-env.sh po/extract.sh
```

This rebuilds `po/kuickshare.pot` from the files in `po/POTFILES.in` and merges it into `po/<language>.po` for each code in `po/LINGUAS`. A new language is a new `po/<language>.po` plus its code in `po/LINGUAS`.

The maintainer's `.claude/` setup has a `translator` agent that fills the new entries (see [Claude Code helpers](#claude-code-helpers)).

## Claude Code helpers

`.claude/` holds the maintainer's [Claude Code](https://claude.com/claude-code) setup; contributors can ignore it. Details in [`.claude/CLAUDE.md`](.claude/CLAUDE.md).

| Kind | Name | Purpose |
|---|---|---|
| Agent | `translator` | Fills the pending entries of `po/*.po` |
| Agent | `phone-log-analyzer` | Reads a hand-test session in `dist/logs` |
| Agent | `rqs-reviewer` | Reviews changes to the Bluetooth and Wi-Fi transfer code |
| Skill | `check` | Runs every check of [Checks](#checks) |
| Skill | `update-translations` | Regenerates the catalogs and runs `translator` |
| Skill | `phone-test` | Guides and analyses a hand test with the phones |

## Releases

### Making a release

1. Set the new version in `Cargo.toml`, `src/shell/CMakeLists.txt` and the `<releases>` block of `data/io.github.the_beper.KuickShare.metainfo.xml`, where the release notes go in the `<description>` of the new `<release>`: the `<li>` items become the "What's Changed" list of the GitHub release (keep them short, one change each), followed by the "Full Changelog" link to the previous `<release>`'s tag. The introductory `<p>` is only for software centers.
2. Push the commit to `main` and wait until the CI run on it is green (its `required` check).
3. Tag that commit `vX.Y.Z`, matching the version, and push only the tag.
4. Approve the `release-signing` environment when the run asks for it: it asks only after `check-flatpak` is green.

Do not create the release in the GitHub UI or with `gh release create`: the release workflow creates a draft, uploads the files and publishes it last. A release you publish by hand is public without its files (the AppImage update link would point at nothing); if one exists, the workflow only adds what is missing and warns.

The tag triggers [`release.yml`](.github/workflows/release.yml), one run for one commit (`github.sha`), one release at a time:

| Job | What it does | Rights |
|---|---|---|
| `validate` | the tag is `vX.Y.Z`, equals the version in `Cargo.toml`, `CMakeLists.txt` and the first metainfo `<release>`, points to the commit of the run, which is on `main`, and is the newest `vX.Y.Z` tag of the repository: a release below an existing one is refused (`build/release/check-version.sh`; other `v*` tags, such as a prerelease, do not count) | read |
| `ci-gate` | waits up to 20 minutes for the CI run of the push of that commit to `main` to finish green (a pull request's run does not count) | read |
| `build-appimage`, `build-tarball`, `build-flatpak` | build and smoke-test with the version of the tag, in the images pinned by digest ([Pinned tools](#pinned-tools)); the Flatpak commits are exported unsigned onto an anonymous clone of `gh-pages` | read; no secret |
| `verify` | exactly the four files, their hashes against the build jobs, the tarball layout and `VERSION`, the `.zsync` header against the AppImage; writes `SHA256SUMS` (those four files) | read |
| `prepare-flatpak` | everything that parses what the build made, without the key (the script refuses to run with it): checks the commits archive against the hash of the build, puts the commits on a clone of `gh-pages` (parent chain), finishes the repository unsigned (summary, appstream data, `.flatpakrepo`; no static deltas) and lists the refs to sign. The published summary and ref heads must verify with `build/flatpak/signing-key.asc` before anything is built on them, and the refs of the clone must be exactly the refs and commits of the signed summary (ostree takes a ref's commit from the summary, so a moved `refs/heads/X` under an old summary would pass a pull alone; a gh-pages that was altered is not re-signed), the `GPGKey` is `signing-key.asc` dearmored, and a published one with other bytes is refused; `gh-pages` must not be older than the newest published release, or someone who can push only to `gh-pages` could bring back an older signed state: once the base verifies, the version of its app head is read from the signed metainfo (first `<release>`) and must not be below the newest non-draft `vX.Y.Z` release that has the Flatpak bundle (listed with `gh`; a draft, a prerelease or a release made by hand without files is no anchor), and a missing `gh-pages` is refused while such a release exists (`build/release/prepare-flatpak-repo.sh`) | read (the job's read-only token, which only `gh` sees); no secret |
| `check-flatpak` | after `prepare-flatpak`, on a keyless machine of its own (no environment, no secret, the plain runner with flatpak and ostree from Ubuntu's signed packages): everything that is checked before the key. It checks the hashes, that `gh-pages` is still where the build left it, the sign list against the build's `refs.txt`, that each listed commit is new, has no signature file and says it belongs to its own ref (`ostree.ref-binding`), that the repository `config` has only the lines ostree writes, that there are no static deltas and `ostree fsck` passes, and that against an anonymous clone of `gh-pages` at the base of the build every unlisted ref is unchanged and none dropped or new, and every listed ref (appstream ones included) continues its published commit and is dated between it and an hour after now; every unlisted head has the `.commit` and `.commitmeta` files of the clone; the appstream commits, which `prepare-flatpak` makes, have exactly the metadata keys `ostree.collection-binding` and `ostree.ref-binding`, an empty collection binding, the subject `Update` and no body; the top of `repo/` has only the entries ostree writes and every file below it is an ostree object, a ref head, a summary file or the config (ostree ignores any other, the site would publish it); member names outside `A-Za-z0-9._/-` are refused; it deletes the old summary files, cleans `repo/tmp` and checks the entries and files of the tree it archives again; `signing-key.asc` must have a readable fingerprint. It writes `checked-repo.tar`, made by its own `tar` from the tree it checked (`repo/` only), and the sign list; the job outputs are the hashes of both and the id of the artifact, the pins of the next job (`build/release/sign-flatpak-repo.sh --check`) | read; no secret |
| `sign-flatpak` | after `ci-gate`, `verify` and `check-flatpak`, so the environment approval is asked for only when every check is green; the only job with an environment (`release-signing`) and the signing key, on the plain runner with flatpak and ostree from Ubuntu's signed packages, not in a container image. It reads nothing of the run but the artifact of `check-flatpak`, downloaded by its id. A first step without the key (`--unpack`: no ostree, gpg, flatpak, git or network, and not in the step with the secret, whose environment any child process could read) checks both files against the hashes `check-flatpak` reported (an empty or malformed hash is refused), that the archive holds only plain files and directories under `repo/` and that the sign list is well formed, and unpacks into an empty folder. The step with the key (a keyring in `/dev/shm`, exactly one secret key, the one of `build/flatpak/signing-key.asc` in the checkout; the secret leaves the environment of the script before any other command runs) signs each listed commit with `ostree gpg-sign` and the summary with `flatpak build-update-repo --no-update-appstream --gpg-sign` (ostree and flatpak still read the commit objects, their `xa.metadata` and the config with the key at hand, hence the checks before), refuses if any file other than the summary files and the signatures of the listed commits is new or has another size, scans those files for key material, and returns the whole repository (`build/release/sign-flatpak-repo.sh --unpack` and `--sign`) | read; key |
| `finish-flatpak` | another machine, no secret: the signed repository has the refs and commits of the prepared one and differs from it only in summary files and signatures of the listed commits; the same comparisons with `gh-pages` and of the appstream commits and the files of `repo/` again, on the signed repository; every file outside `repo/` is exactly what `build/flatpak/write-site-files.sh` writes for `SITE_URL` and the key; a client with only the public key accepts it (`flatpak install --no-deploy` and an `ostree pull` with signature checks of every listed ref), the `GPGKey` is `signing-key.asc` dearmored, byte for byte, and nothing but directories lies under `refs/mirrors` (no ref, no link); writes `site.tar` (`build/release/finish-flatpak-repo.sh`) | read |
| `publish` | draft release, missing assets, `gh-pages`, wait for Pages, then publishes (`build/release/publish.sh`) | write |

`build/release/simulate.sh` runs these scripts against fakes of `gh`, `flatpak`, `ostree` and `curl` with ephemeral GnuPG keys (CI runs it in `lint`). Order and states: assets are uploaded to the draft first, then `gh-pages` is pushed as one orphan commit (the old one stays as the branch `gh-pages-previous`; one `git push --atomic` with a lease on both branches, so a push that fails moves nothing; both branches are then read again, and exit 3 means a writer got in meanwhile, exit 1 that none did), then Pages is polled until it serves the new `summary` and `summary.sig`, then the draft is published as latest. The Release and the Flatpak repository are not one transaction; the only visible gap is some minutes where Flatpak users have the new version before the Release is public.

### When a run fails

- Fix a transient problem and use "Re-run failed jobs": `publish` is idempotent (same asset content is skipped, a different one stops the run, nothing is overwritten; `gh-pages` is pushed only if it is not already the new site). To rebuild, "Re-run all jobs".
- `stale base` (exit 3 in `prepare-flatpak`, `check-flatpak`, `finish-flatpak` or `publish`; `gh-pages` can also move while the approval waits, and `finish-flatpak` then refuses), or `the push to gh-pages failed because another writer changed gh-pages` (exit 3 in `publish`): another release changed `gh-pages` after the build; re-run all jobs.
- `the app of gh-pages is at X.Y.Z, but vA.B.C is published`, or `there is no gh-pages, but vA.B.C is published` (exit 1 in `prepare-flatpak`): `gh-pages` was brought back to an older state, deleted, or the release was published without its Pages update. Restore it before a new release: `git fetch origin <commit> && git push --force origin <commit>:gh-pages` (a fresh clone does not have the commit). `<commit>` is the one in the line `publish: gh-pages is now <commit>` of the log of the `publish` job of the run of vA.B.C (logs stay about 90 days), or the `commit` of the Pages build of that release in `gh api repos/the-beper/KuickShare/pages/builds` (inferred from the API documentation, not tried). Re-running the `publish` job of that run restores it too, but only while `gh-pages` is still the commit that run's build started from (else `publish` exits 3) and while its artifacts are kept (14 days; a run can be re-run for 30). If none of this is left, or the release was published by hand after a failed push, delete the Flatpak bundle from that release (`gh release delete-asset vA.B.C KuickShare-x86_64.flatpak`): it is then no anchor and the next release builds on the `gh-pages` that is there, accepting the rollback. The anchor falls to the next older release that has a bundle, so this may have to be repeated.
- `cannot list the releases of the-beper/KuickShare` (exit 2 in `prepare-flatpak`, usually a transient error of the API) and `cannot read the version of the signed app head of gh-pages` (exit 1: the signed app head has no readable metainfo while a release is published): use "Re-run failed jobs"; the second one repeats unless `gh-pages` or the build is wrong.
- `the push to gh-pages failed and neither branch has moved` (exit 1 in `publish`): a network or server error, or a ruleset refusing the push; nothing was pushed. Fix the cause and use "Re-run failed jobs".
- `the newest release tag is vX.Y.Z` (`validate`): a newer release tag exists, so this tag cannot be released. If the newer tag was a mistake and has no release, delete it and re-run; otherwise cut the next version.
- `ci-gate` fails: CI is red or missing on the tagged commit. Fix on `main`, then tag the new commit. Do not move a tag that was pushed: delete it and push a new one only if no release exists for it.
- An asset with different content on the release (`refusing to overwrite`): look at why; delete that asset by hand only if it is the wrong one, then re-run `publish`.
- Rollback of a bad Flatpak repository: `git fetch origin gh-pages-previous && git push --force origin FETCH_HEAD:gh-pages`, then `gh release edit vX.Y.Z --draft=true`. Clients that already updated keep the new commit; fix forward with a new version.

### Pinned tools

The release builds with fixed tool versions, bumped by hand in a pull request. Dependabot does not see them.

| Tool | Pin | Where | Re-check |
|---|---|---|---|
| Rust of the tarball | `1.99.1` | `RUST_VERSION` in `release.yml`, and CI's tarball job (`check.sh` keeps them equal) | run `build/build-env.sh build/check.sh` with that toolchain; the `rustup-init` script itself is not pinned, it only installs this toolchain |
| Rust of the Flatpak | `org.freedesktop.Sdk.Extension.rust-stable//25.08` at `09784a99…1507` (rustc 1.98.1) | `rust_commit` in `build/flatpak/build-flatpak.sh` | below |
| KDE SDK and runtime | `org.kde.Sdk//6.11` at `df314f9c…3a86`, `org.kde.Platform//6.11` at `76fa01ac…c473` | `sdk_commit`, `platform_commit` in the same file | below |
| Build images | `ghcr.io/the-beper/kuickshare-build:fedora45`, `…-flatpak:fedora45`, `…-appimage:arch`, each `@sha256:…` | the image references of the build jobs in `release.yml` (`check.sh` fails on one without a digest) | [Bump the images](#bump-the-build-images) |
| `tomlkit` of the Flatpak image | exact version and wheel hash (`--require-hashes`) | `build/flatpak/Containerfile` | the hash is `urls[].digests.sha256` of the wheel in `https://pypi.org/pypi/tomlkit/<version>/json`; after a change of the pin, the release uses it only once `images.yml` has rebuilt `kuickshare-flatpak` on `main` and its digest in `release.yml` is bumped |

The Flatpak build installs the three refs, moves each to its pinned commit and stops if one is elsewhere, before and after `flatpak-builder`, which runs without `--install-deps-from` so it cannot update them. The extension branch (`25.08`) is the freedesktop version of the KDE SDK (`flatpak remote-info --show-metadata flathub org.kde.Sdk//6.11`, `[Extension org.freedesktop.Sdk.Extension]`). The freedesktop runtime and the other extensions the KDE runtime pulls in are not pinned, and neither is the AppImage's Rust (the Arch image has it, at whatever version the image was built with).

Re-check or bump a commit with a throwaway Flatpak directory, so nothing touches the real installation:

```sh
export FLATPAK_USER_DIR=$(mktemp -d)
flatpak --user remote-add flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak --user remote-info --arch=x86_64 --show-commit flathub org.kde.Sdk//6.11
```

After a bump run `build/flatpak/build-flatpak.sh` and `build/flatpak/test-flatpak.sh dist/KuickShare-x86_64.flatpak`.

#### Bump the build images

`images.yml` rebuilds the three images weekly and on a change to a Containerfile, under the same tag; the release names an image by digest, so a rebuild changes nothing in a release until the digest is bumped by hand. Dependabot cannot do it: it is not known to read the `container:` image of a workflow (inferred, not verified). Old digests stay pullable from `ghcr.io` *(inferred)*.

1. After `images.yml` has run on `main`, take the new `image:tag@sha256:…` lines from the `Digest` step of each job summary (Actions, Images). Or ask the registry, read-only and without credentials (`kuickshare-build` and `fedora45` are the image and tag): `curl -fsSI -H "Authorization: Bearer $(curl -fsS 'https://ghcr.io/token?scope=repository:the-beper/kuickshare-build:pull' | jq -r .token)" -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' https://ghcr.io/v2/the-beper/kuickshare-build/manifests/fedora45 | grep -i docker-content-digest`.
2. Replace the digests in `release.yml` (every occurrence of the three images) in a pull request, and run `build/build-env.sh build/check.sh`.

A change to a Containerfile therefore takes two pull requests: the change, then the digest bump after `images.yml` published it. Until then the release builds in the previous image.

### One-time setup (needs a repository admin)

These are GitHub settings; nothing in the workflows can do them.

1. Before the first run of this workflow, and before deleting the repository secret: create the environment `release-signing` (Settings, Environments), deployment branches and tags limited to the tag rule `v*`, required reviewer `the-beper` (self-review allowed). Put the signing key in it: `gh secret set FLATPAK_GPG_PRIVATE_KEY --env release-signing` with the armored secret key without a passphrase, from the offline copy of the key (its fingerprint must be the one of `build/flatpak/signing-key.asc`, `716F76876E67220B72383C99D141D5EC71EE9E68`; the job refuses any other). A new key means every user must re-add the remote. **A job that names an environment that does not exist makes GitHub create it without protection, and the repository secret of the same name stays readable by every workflow until it is deleted.**
2. Right after step 1, and before the first run of this workflow: `gh secret delete FLATPAK_GPG_PRIVATE_KEY` (the repository-level secret; the key now lives only in the environment).
3. Ruleset for tags `v*`: block updates, deletions and non-fast-forward pushes; only admins may create them.
4. Ruleset for `main`: required status check `required` (source GitHub Actions), no force-push, no deletion (admin bypass if you keep pushing to `main`).
5. Settings, Actions: keep the default `GITHUB_TOKEN` read-only and the approval for first-time contributors; optionally require full-length commit SHAs for actions.
6. Settings, Pages: deploy from the `gh-pages` branch (root), after the first release.

A renewed key (same fingerprint, new bytes, for example a later expiry) is not a new key, but replacing `build/flatpak/signing-key.asc` alone stops every release: `prepare-flatpak` refuses a published `GPGKey` that is not the bytes of the file, and only a release writes `gh-pages`. Before the next tag, with no release in between:

1. Put the renewed secret key into the environment as in One-time setup, step 1 (the job compares the fingerprint only).
2. Merge the new `signing-key.asc`.
3. Commit the new bytes to `gh-pages` by hand: in `kuickshare.flatpakrepo` and `kuickshare.flatpakref` replace the value of `GPGKey=` with the output of `gpg --dearmor < build/flatpak/signing-key.asc | base64 -w0`, and change nothing else.

The next release then passes and keeps the bytes (`simulate.sh` runs these steps with fakes). Installed clients keep the key they imported; whether they take the new expiry from the `.flatpakrepo` is not verified.

Tool versions: the jobs that prepare, sign and finish the repository are pinned to `ubuntu-24.04` and use its flatpak and ostree. The release's `build-flatpak` job and CI's `flatpak` job run on the same image, and `build-flatpak` also installs Ubuntu's ostree for the commit export, so podman, flatpak and ostree do not change between build, signing and the update test.

There are no static deltas: a delta carries the signature of its commit and the signed summary has to list it, so deltas would have to be made with the key present or the summary signed twice. An update downloads the changed objects instead; `publish-repo.sh` deletes the deltas and delta indexes of earlier releases (the published repository has some) before it makes the summary. Bringing deltas back means a keyless job that makes them from the signed repository and a second signing step for the summary only. CI's `repo-update` job runs `build/flatpak/test-repo-update.sh` on the same runner image with the same apt packages (plus `librsvg2-common`, which only the test's re-export needs), on the repository the `flatpak` job built; it makes the new site with the three scripts above and an ephemeral key, so the proof that installed clients update covers the versions releases use. A runner image or package update can still change them: that job going red is the warning. The first release must still be checked with a real client (below).

The `release` concurrency group keeps one running and one pending run: if three tags are pushed in quick succession, GitHub cancels the middle one, and it cannot be released any more (`validate` refuses a tag older than the newest one): cut the next version instead.

Not tested with the real tools: the fakes in `simulate.sh` stand in for `gh`, `flatpak`, `ostree` and Pages (so the checks of `finish-flatpak-repo.sh` that name a file set, `summary*`, `summaries/*` and `*.commitmeta`, are the set Flatpak 1.14.6 and ostree 2024.5 produced when tried by hand (as are the metadata keys, subject and body of the appstream commits, the entries at the top of `repo/` and the file names below it), and a runner image update can change it: `repo-update` going red is the warning). `build/flatpak/test-repo-update.sh` (CI, `repo-update` job) drives a real Flatpak client with an ephemeral key. After the first real release check by hand with the real key: the secret imports and has fingerprint `716F…9E68`; the `GPGKey` of the new `kuickshare.flatpakrepo` is byte-identical to the old one; an existing install updates (`flatpak remote-info --log kuickshare io.github.the_beper.KuickShare` shows the old commit as parent).

## Proposing changes

1. Fork, create a branch, make the change.
2. Make sure all checks pass.
3. Open a pull request describing what and why.

By contributing you agree that your work is licensed under [GPL-3.0-or-later](LICENSE).
