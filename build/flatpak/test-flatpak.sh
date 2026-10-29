#!/bin/sh
# Smoke test of the Flatpak bundle in a throw-away container (no desktop: offscreen Qt, a private session bus,
# a fake tray watcher, the document portal but no desktop portal): install with non-default XDG folders, start,
# check the core answers on D-Bus, the tray item registers, the Dolphin entry lands in the host folder, and file
# forwarding hands a file to the sandbox. Notifications, job progress, the file chooser and Dolphin itself need a
# real Plasma session.
# Usage: [KUICKSHARE_EXPECT_VERSION=1.2.3] build/flatpak/test-flatpak.sh dist/KuickShare-x86_64.flatpak
# KUICKSHARE_EXPECT_VERSION (optional): the version `--version` must print. main.cpp has no flag of its own: it comes
# from KAboutData::setupCommandLine, which prints "kuickshare <version>" (a "kuickshare: <version>" line is accepted too).
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
image=localhost/kuickshare-flatpak:fedora45

if [ ! -e /run/.containerenv ]; then
    bundle=$(readlink -f "$1")
    exec podman run --rm --privileged --security-opt label=disable -e KUICKSHARE_EXPECT_VERSION \
        --device /dev/fuse -v kuickshare-flatpak:/var/lib/flatpak -v "$bundle:/bundle.flatpak:ro" -v "$root/build/flatpak/test-flatpak.sh:/test.sh:ro" \
        -e XDG_RUNTIME_DIR=/run/user/0 \
        "$image" sh -c 'mkdir -p -m 700 /run/user/0 && exec dbus-run-session -- /test.sh /bundle.flatpak'
fi

id=io.github.the_beper.KuickShare
fail() { echo "FAIL: $*" >&2; exit 1; }
HOME=$(mktemp -d)
export HOME
trap 'flatpak kill "$id" 2>/dev/null; kill %1 2>/dev/null; rm -rf "$HOME"' EXIT
# Non-default XDG folders on the host, as some users have.
export XDG_DATA_HOME=$HOME/xdg-data XDG_CONFIG_HOME=$HOME/xdg-config
mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME"

flatpak install --user -y --noninteractive --bundle "$1" >/dev/null 2>&1 || fail "install"
echo "ok: installed"
if [ -n "${KUICKSHARE_EXPECT_VERSION:-}" ]; then
    out=$(flatpak run --env=QT_QPA_PLATFORM=offscreen "$id" --version 2>&1) || fail "--version failed: $out"
    printf '%s\n' "$out" | grep -qxF -e "kuickshare $KUICKSHARE_EXPECT_VERSION" -e "kuickshare: $KUICKSHARE_EXPECT_VERSION" \
        || fail "--version is not $KUICKSHARE_EXPECT_VERSION: $out"
    echo "ok: version $KUICKSHARE_EXPECT_VERSION"
fi

# A minimal StatusNotifierWatcher that records what registers with it.
python3 - >"$HOME/watcher.log" 2>&1 <<'PY' &
import sys
from gi.repository import Gio, GLib
XML = """<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
</interface></node>"""
def call(conn, sender, path, iface, method, params, inv):
    print(method, params.unpack(), sender, flush=True)
    inv.return_value(None)
def prop(conn, sender, path, iface, name):
    return {"IsStatusNotifierHostRegistered": GLib.Variant("b", True), "ProtocolVersion": GLib.Variant("i", 0),
            "RegisteredStatusNotifierItems": GLib.Variant("as", [])}[name]
def acquired(conn, name):
    conn.register_object("/StatusNotifierWatcher", Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0], call, prop, None)
Gio.bus_own_name(Gio.BusType.SESSION, "org.kde.StatusNotifierWatcher", Gio.BusNameOwnerFlags.NONE, acquired, None, None)
GLib.MainLoop().run()
PY
sleep 1

flatpak run --env=QT_QPA_PLATFORM=offscreen "$id" --background >"$HOME/app.log" 2>&1 &
owner() {
    gdbus call --session --dest "$id" --object-path /io/github/the_beper/KuickShare \
        --method org.freedesktop.DBus.Properties.Get io.github.the_beper.KuickShare1 DeviceName 2>/dev/null
}
for _ in $(seq 20); do owner >/dev/null && break; sleep 1; done
owner >/dev/null || { cat "$HOME/app.log"; fail "the core did not answer on D-Bus"; }
echo "ok: started, core answers: $(owner)"

menu=$XDG_DATA_HOME/kio/servicemenus/kuickshare-send.desktop
for _ in $(seq 10); do [ -e "$menu" ] && break; sleep 1; done
[ -x "$menu" ] || { cat "$HOME/app.log"; fail "Dolphin entry missing or not executable"; }
grep -q "^Exec=flatpak run --file-forwarding $id @@ %F @@$" "$menu" || fail "wrong Exec in the Dolphin entry: $(grep ^Exec= "$menu")"
echo "ok: Dolphin entry written for flatpak run, in the host's XDG_DATA_HOME"

for _ in $(seq 10); do grep -q RegisterStatusNotifierItem "$HOME/watcher.log" && break; sleep 1; done
grep -q RegisterStatusNotifierItem "$HOME/watcher.log" || fail "the tray item did not register with the watcher"
echo "ok: tray item registered"

echo hello > "$HOME/hello.txt"
out=$(flatpak run --file-forwarding --command=sh "$id" -c 'cat "$1"' _ @@ "$HOME/hello.txt" @@ 2>&1) || true
[ "$out" = hello ] || fail "file forwarding did not deliver the file: $out"
echo "ok: file forwarding"

# A second launch (what Dolphin does) must hand the file to the running instance and exit.
timeout 30 flatpak run --env=QT_QPA_PLATFORM=offscreen --file-forwarding "$id" @@ "$HOME/hello.txt" @@ >/dev/null 2>&1 \
    || fail "the second launch did not hand over to the running instance"
echo "ok: second launch hands over"

[ -e "$HOME/.var/app/$id/data/kuickshare/identity.json" ] || fail "no identity under ~/.var/app"
echo "ok: data under ~/.var/app"

flatpak kill "$id"
echo "done"
