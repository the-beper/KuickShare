#!/bin/sh
# Installs the program built against the system's Qt 6 / KDE Frameworks 6 into ~/.local, or removes it. Run by
# the tarball's install.sh:
#   install.sh [--no-launch] [files...]         install or update, then start the app (files are queued on the Send page)
#   install.sh --uninstall                      remove the program; user data is kept
#   install.sh --uninstall --purge              also remove settings, history and paired devices
set -eu

data=${XDG_DATA_HOME:-$HOME/.local/share}
config=${XDG_CONFIG_HOME:-$HOME/.config}
app=$data/kuickshare
bin=$app/bin
appid=io.github.the_beper.KuickShare

# A path as a double-quoted Exec argument, with the escapes the Desktop Entry specification wants (as quoteForExec in
# src/shell/execquote.h): inside the quotes " ` $ and \ get a backslash (the backslash is doubled again for the
# file) and % is %%.
exec_quote() {
    printf '"%s"' "$(printf '%s' "$1" | sed -e 's/\\/\\\\\\\\/g' -e 's/["`$]/\\&/g' -e 's/%/%%/g')"
}

# Copies a desktop file with its Exec line replaced (ENVIRON, not awk -v, keeps the backslashes as they are).
write_desktop() {
    EXEC_LINE="Exec=$3" awk '/^Exec=/ { print ENVIRON["EXEC_LINE"]; next } { print }' "$1" > "$2"
}

# A path as a single-quoted shell word.
sh_quote() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# The shell and the service, whichever way they were started (the symlink in ~/.local/bin, the menu, autostart).
running() {
    pgrep -u "$(id -u)" -x kuickshare >/dev/null 2>&1 || pgrep -u "$(id -u)" -x kuickshared >/dev/null 2>&1
}

# Returns 0 if something was running and is now stopped, 1 if nothing was running.
stop_running() {
    running || return 1
    pkill -TERM -u "$(id -u)" -x kuickshare || true
    pkill -TERM -u "$(id -u)" -x kuickshared || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        running || return 0
        sleep 0.5
    done
    pkill -KILL -u "$(id -u)" -x kuickshare || true
    pkill -KILL -u "$(id -u)" -x kuickshared || true
    return 0
}

refresh_caches() {
    touch "$data/icons" "$data/icons/hicolor" "$data/icons/hicolor/scalable" "$data/icons/hicolor/scalable/apps" \
        "$data/icons/hicolor/symbolic" "$data/icons/hicolor/symbolic/apps" 2>/dev/null || true
    command -v kbuildsycoca6 >/dev/null 2>&1 && kbuildsycoca6 >/dev/null 2>&1 || true
}

uninstall() {
    stop_running || true
    rm -f "$bin/kuickshare" "$bin/kuickshared" "$app/version" "$app/install.sh" "$app/uninstall.sh"
    rmdir "$bin" 2>/dev/null || true
    rm -f "$data/applications/$appid.desktop" \
        "$data/metainfo/$appid.metainfo.xml" \
        "$data/kio/servicemenus/kuickshare-send.desktop" \
        "$data/knotifications6/kuickshare.notifyrc" \
        "$data"/locale/*/LC_MESSAGES/kuickshare.mo \
        "$data/icons/hicolor/scalable/apps/$appid.svg" \
        "$data/icons/hicolor/symbolic/apps/$appid-symbolic.svg" \
        "$config/autostart/$appid.desktop"
    # The link in ~/.local/bin only if it is ours.
    [ "$(readlink "$HOME/.local/bin/kuickshare" 2>/dev/null)" = "$bin/kuickshare" ] && rm -f "$HOME/.local/bin/kuickshare"
    if [ "${1:-}" = "--purge" ]; then
        rm -rf "$config/kuickshare" "$app"
        echo "KuickShare removed, including settings, history and paired devices."
    else
        rmdir "$app" 2>/dev/null || true
        echo "KuickShare removed. Settings, history and paired devices are kept in $app and $config/kuickshare (use --uninstall --purge to delete them)."
    fi
    refresh_caches
}

if [ "${1:-}" = "--uninstall" ]; then
    shift
    uninstall "$@"
    exit 0
fi

launch=1
if [ "${1:-}" = "--no-launch" ]; then
    launch=0
    shift
fi

here=$(cd "$(dirname "$0")" && pwd)
APPDIR=${APPDIR:-$(cd "$here/../../.." && pwd)}
src=$APPDIR/usr/share/kuickshare
hostbin=$src/host/bin

missing=$(ldd "$hostbin/kuickshare" 2>&1 | grep 'not found' | sed 's/^[[:space:]]*//')
for module in org/kde/kirigami org/kde/kirigamiaddons/formcard; do
    found=0
    for dir in /usr/lib64/qt6/qml /usr/lib/qt6/qml /usr/lib/x86_64-linux-gnu/qt6/qml; do
        [ -d "$dir/$module" ] && found=1
    done
    [ "$found" = 1 ] || missing="$missing
QML module $module"
done
if [ -n "$missing" ]; then
    echo "KuickShare needs these on the system:" >&2
    echo "$missing" >&2
    echo "On Fedora: sudo dnf install kf6-kirigami kf6-kirigami-addons qt6-qtdeclarative" >&2
    echo "On Arch:   sudo pacman -S kirigami kirigami-addons qt6-declarative" >&2
    exit 1
fi

restart=0
if stop_running; then
    restart=1
fi

mkdir -p "$bin" "$data/applications" "$data/metainfo" "$data/kio/servicemenus" "$data/knotifications6" \
    "$data/icons/hicolor/scalable/apps" "$data/icons/hicolor/symbolic/apps" "$HOME/.local/bin"

for file in kuickshare kuickshared; do
    cp "$hostbin/$file" "$bin/$file.new"
    chmod 755 "$bin/$file.new"
    mv -f "$bin/$file.new" "$bin/$file"
done
cp "$src/install.sh" "$app/install.sh"
printf '#!/bin/sh\nexec %s --uninstall "$@"\n' "$(sh_quote "$app/install.sh")" > "$app/uninstall.sh"
chmod 755 "$app/install.sh" "$app/uninstall.sh"
cp "$src/VERSION" "$app/version"

write_desktop "$src/$appid.desktop" "$data/applications/$appid.desktop" "$(exec_quote "$bin/kuickshare") %U"
# Settings -> Integration -> Dolphin context menu: an update must not bring it back once it was turned off.
if [ -f "$config/kuickshare/settings.json" ] && grep -Eq '"dolphin_integration"[[:space:]]*:[[:space:]]*false' "$config/kuickshare/settings.json"; then
    rm -f "$data/kio/servicemenus/kuickshare-send.desktop"
else
    write_desktop "$src/kuickshare-send.desktop" "$data/kio/servicemenus/kuickshare-send.desktop" "$(exec_quote "$bin/kuickshare") %F"
fi
cp "$src/$appid.metainfo.xml" "$data/metainfo/$appid.metainfo.xml"
cp "$src/kuickshare.notifyrc" "$data/knotifications6/kuickshare.notifyrc"
# Translations: <lang>/LC_MESSAGES/kuickshare.mo, found by KDE under the user's data directory.
if [ -d "$src/locale" ]; then
    mkdir -p "$data/locale"
    cp -r "$src/locale/." "$data/locale/"
fi
cp "$src/icons/$appid.svg" "$data/icons/hicolor/scalable/apps/$appid.svg"
cp "$src/icons/$appid-symbolic.svg" "$data/icons/hicolor/symbolic/apps/$appid-symbolic.svg"
# Do not replace a kuickshare of the user's own in ~/.local/bin: only a missing one or our own link.
link="$HOME/.local/bin/kuickshare"
if { [ ! -e "$link" ] && [ ! -L "$link" ]; } || [ "$(readlink "$link" 2>/dev/null)" = "$bin/kuickshare" ]; then
    ln -sf "$bin/kuickshare" "$link"
else
    echo "Not replacing $link: it is not a link to this installation." >&2
fi

refresh_caches
echo "KuickShare $(cat "$app/version") installed in $app. The files you installed from can be deleted."
echo "To remove it: $app/uninstall.sh"

if [ "$launch" = 1 ] || [ "$restart" = 1 ]; then
    # Run by the AppImage, this script inherits its environment: the installed program must not see it (APPIMAGE
    # makes the shell act as the AppImage and write entries that point at the .AppImage file).
    unset APPIMAGE APPDIR ARGV0 OWD
    setsid nohup "$bin/kuickshare" "$@" >/dev/null 2>&1 &
fi
