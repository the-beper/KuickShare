# shellcheck shell=bash disable=SC2034
# Source it. Gives the test scripts a throwaway home in RAM (/dev/shm), so a test run never touches the real settings, history, trusted devices or received files:
#   env "${SANDBOX_ENV[@]}" <command>   run a command in the sandbox
#   sandbox_cleanup                     delete it (call it when the test ends)
# Sourcing it also deletes the sandboxes of earlier runs that were not cleaned up (the script was killed): those whose owner, recorded in `pid`, is no longer running.
# Only the daemon and the shell run in it: adb, nmcli and the other tools keep the real home.
# XDG_RUNTIME_DIR and the session bus stay real, so tray, notifications and job tracker work.
SANDBOX=$(mktemp -d /dev/shm/kuickshare-test.XXXXXX) || exit 1
mkdir -p "$SANDBOX/home/.config" "$SANDBOX/home/.local/share" "$SANDBOX/home/.cache" "$SANDBOX/tmp"
# The application's own icons, as an installed copy would provide them: without them the window and the tray fall back to a stock icon.
mkdir -p "$SANDBOX/home/.local/share/icons"
cp -r "$(dirname "${BASH_SOURCE[0]}")/../../data/icons/hicolor" "$SANDBOX/home/.local/share/icons/"
[ -f "${XDG_CONFIG_HOME:-$HOME/.config}/kdeglobals" ] &&
    cp "${XDG_CONFIG_HOME:-$HOME/.config}/kdeglobals" "$SANDBOX/home/.config/"
echo $$ > "$SANDBOX/pid"
SANDBOX_ENV=(
    HOME="$SANDBOX/home"
    XDG_CONFIG_HOME="$SANDBOX/home/.config"
    XDG_DATA_HOME="$SANDBOX/home/.local/share"
    XDG_CACHE_HOME="$SANDBOX/home/.cache"
    TMPDIR="$SANDBOX/tmp"
)
sandbox_cleanup() {
    case $SANDBOX in /dev/shm/kuickshare-test.*) rm -rf -- "$SANDBOX" ;; esac
}

# A sandbox of another run is stale when it has no `pid` (older version of this file) or that process is gone.
sandbox_prune() {
    local dir pid
    for dir in /dev/shm/kuickshare-test.*; do
        [ -d "$dir" ] && [ "$dir" != "$SANDBOX" ] || continue
        pid=$(cat "$dir/pid" 2>/dev/null) || pid=
        if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
            rm -rf -- "$dir"
        fi
    done
}
sandbox_prune
