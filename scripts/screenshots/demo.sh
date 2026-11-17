#!/bin/sh
# Runs the real shell with made-up data: a fake core (fake_core.py) answers on a private D-Bus bus,
# and settings, data and cache go to a throw-away directory. Nothing of the real session is touched.
# Usage: scripts/screenshots/demo.sh [kuickshare options and files...]
# The shell binary is dist/tmp/cmake-dev/kuickshare unless KUICKSHARE_BIN says otherwise.
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin=${KUICKSHARE_BIN:-$root/dist/tmp/cmake-dev/kuickshare}
[ -x "$bin" ] || { echo "no shell binary at $bin (build it first, see CONTRIBUTING.md)" >&2; exit 1; }

mkdir -p "$root/dist/tmp"
tmp=$(mktemp -d "$root/dist/tmp/demo.XXXXXX")
mkdir -p "$tmp/config" "$tmp/data/icons" "$tmp/cache" "$tmp/files"
# The light Breeze look the documentation images have, whatever the desktop uses; without that colour scheme
# file, the desktop's own colours and icon theme. The project's own icons are added either way.
light=/usr/share/color-schemes/BreezeLight.colors
if [ -r "$light" ]; then
    { cat "$light"; printf '\n[Icons]\nTheme=breeze\n'; } > "$tmp/config/kdeglobals"
else
    cp "${XDG_CONFIG_HOME:-$HOME/.config}/kdeglobals" "$tmp/config/" 2>/dev/null
fi
cp -r "$root/data/icons/hicolor" "$tmp/data/icons/"
for f in photo-1.jpg:2400000 photo-2.jpg:3100000 notes.pdf:850000; do
    truncate -s "${f#*:}" "$tmp/files/${f%:*}"
done

address="unix:abstract=kuickshare-demo-$$"
cat > "$tmp/bus.conf" <<CONF
<busconfig><type>session</type><listen>$address</listen>
<policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy></busconfig>
CONF
dbus-daemon --config-file="$tmp/bus.conf" --nofork &
bus=$!
export DBUS_SESSION_BUS_ADDRESS=$address
sleep 0.5
python3 "$here/fake_core.py" 2>/dev/null &
core=$!
sleep 1

# The shell starts the real core if nothing owns the name: stop unless the fake one does.
if gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
    -m org.freedesktop.DBus.NameHasOwner io.github.the_beper.KuickShare | grep -q true; then
    cd "$tmp/files" || exit 1
    QT_FORCE_STDERR_LOGGING=1 LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 LANGUAGE=en \
        XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" XDG_CACHE_HOME="$tmp/cache" \
        "$bin" "$@" &
    shell=$!
    trap 'kill $shell' TERM INT
    wait $shell
    # A signal ends that wait at once; the shell still tells the core its window closed.
    wait $shell
else
    echo "the fake core did not come up" >&2
fi
kill $core $bus 2>/dev/null
rm -rf "$tmp"
