#!/usr/bin/env bash
# Hand test of the Wi-Fi upgrade on a laptop that is on Wi-Fi (one radio): runs the daemon from this
# folder, logs the network, Bluetooth and daemon state to logs/<timestamp>/, and drives it over D-Bus.
# Copy next to the binaries (kuickshared, optionally kuickshare) and run it as your normal user, from a
# terminal in the desktop session or over ssh. The run itself is a transient user service, so it
# survives the ssh session dropping when the laptop joins the phone's network, and NetworkManager and
# firewalld see it as part of the desktop session (a plain ssh process is refused by polkit).
#
#   test-laptop.sh start [--gui] [--accept]  start the daemon (and the GUI), WifiUpgrade on, monitors running;
#                                  --accept accepts incoming transfers by itself (there is no GUI to ask)
#   test-laptop.sh scan [seconds]  look for phones (default 20 s) and print the devices found
#   test-laptop.sh send ID FILE    send FILE to the device ID printed by scan (for example ble:G483)
#   test-laptop.sh status          what is running, the active Wi-Fi connection, leftover profiles
#   test-laptop.sh report          the key events of the latest run
#   test-laptop.sh stop            stop everything (it also stops by itself after $DURATION seconds)
# Settings and history go to ./state, not to the laptop's own KuickShare data.
set -u

DIR=$(cd "$(dirname "$0")" && pwd)
UNIT=kuickshare-test
DURATION=${DURATION:-900}
SERVICE=io.github.the_beper.KuickShare
OBJECT=/io/github/the_beper/KuickShare
IFACE=io.github.the_beper.KuickShare1
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}

[ "$(id -u)" -ne 0 ] || { echo "run as your normal user, without sudo" >&2; exit 1; }
need() { command -v "$1" >/dev/null || { echo "missing command: $1" >&2; exit 1; }; }
call() { gdbus call --session --dest "$SERVICE" --object-path "$OBJECT" --method "$IFACE.$1" "${@:2}"; }
latest() { readlink -f "$DIR/logs/latest" 2>/dev/null; }
running() { systemctl --user is-active --quiet "$UNIT.service"; }

cmd_start() {
    need systemd-run; need gdbus; need nmcli
    [ -x "$DIR/kuickshared" ] || { echo "no $DIR/kuickshared" >&2; exit 1; }
    running && { echo "already running (test-laptop.sh stop first)"; exit 1; }
    if gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
        -m org.freedesktop.DBus.NameHasOwner "$SERVICE" | grep -q true; then
        echo "another KuickShare owns $SERVICE on this session: quit it first" >&2
        exit 1
    fi
    systemd-run --user --unit="$UNIT" --collect \
        -E DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" -E DURATION="$DURATION" \
        -E WITH_GUI="$(printf '%s\n' "$@" | grep -qx -- --gui && echo 1 || echo 0)" \
        -E AUTO_ACCEPT="$(printf '%s\n' "$@" | grep -qx -- --accept && echo 1 || echo 0)" \
        "$DIR/$(basename "$0")" run
    for _ in $(seq 1 20); do
        sleep 1
        call StartDiscovery >/dev/null 2>&1 && break
    done
    echo "logs: $(latest)"
    echo "next: test-laptop.sh scan, then test-laptop.sh send <id> <file>"
}

# Internal: the body of the service.
cmd_run() {
    need nmcli; need journalctl
    local logs
    logs=$DIR/logs/$(date +%Y%m%d-%H%M%S)
    mkdir -p "$logs" "$DIR/state/config" "$DIR/state/data" || exit 1
    ln -sfn "$logs" "$DIR/logs/latest"
    local pids=()
    bg() { local out=$1; shift; "$@" > "$out" 2>&1 & pids+=($!); }
    stop_all() { kill "${pids[@]}" 2>/dev/null; sleep 1; kill -9 "${pids[@]}" 2>/dev/null; }
    trap 'stop_all; exit 0' TERM INT

    {
        date; date -u
        echo "--- nmcli"; nmcli --version; nmcli device status; nmcli -t -f NAME,TYPE connection show
        echo "--- firewalld"; firewall-cmd --state 2>&1; firewall-cmd --get-active-zones 2>&1
        echo "--- bluetooth"; bluetoothctl show 2>&1 | grep -E "Controller|Name|Powered"
        echo "--- rfkill"; rfkill list 2>&1
        echo "--- dnsmasq"; command -v dnsmasq
    } > "$logs/state-before.txt" 2>&1

    # System side. Reading the journal needs the adm or systemd-journal group; btmon and nft need
    # root, so they only run when sudo works without a password.
    bg "$logs/system.log" journalctl -f -o short-precise -u NetworkManager -u bluetooth -u firewalld -u wpa_supplicant
    bg "$logs/kernel.log" journalctl -k -f -o short-precise
    bg "$logs/nm-monitor.log" nmcli monitor
    if sudo -n true 2>/dev/null; then
        bg "$logs/btmon.txt" sudo -n btmon -t
    else
        echo "no passwordless sudo: no btmon, no nat.log" > "$logs/no-sudo.txt"
    fi
    (
        while :; do
            echo "== $(date -u +%T.%N | cut -c1-12) UTC"
            nmcli -t -f NAME,DEVICE connection show --active
            ip -br -4 addr
            # When the phone's group shows up in NetworkManager's scan results (no rescan here)
            nmcli -t -f SSID,FREQ,SIGNAL,BSSID device wifi list --rescan no 2>/dev/null | grep -a '^DIRECT' | sed 's/^/DIRECT-in-list: /'
            sudo -n nft list ruleset 2>/dev/null | grep -E "masquerade|192.168.49" 
            sleep 0.5
        done
    ) > "$logs/net-sampler.log" 2>&1 &
    pids+=($!)

    RUST_LOG=info,rqs_lib::hdl::ble_receiver=debug,rqs_lib::hdl::ble_scan=debug,rqs_lib::hdl::ble_send=debug,rqs_lib::upgrade=debug,kuickshared::link=debug,kuickshared::app=debug \
        XDG_CONFIG_HOME="$DIR/state/config" XDG_DATA_HOME="$DIR/state/data" \
        bg "$logs/daemon.log" "$DIR/kuickshared"
    local tries=0
    until call History >/dev/null 2>&1; do
        tries=$((tries + 1)); [ "$tries" -lt 30 ] || break
        sleep 1
    done
    gdbus call --session --dest "$SERVICE" --object-path "$OBJECT" \
        --method org.freedesktop.DBus.Properties.Set "$IFACE" WifiUpgrade "<true>" > "$logs/set-wifiupgrade.txt" 2>&1
    bg "$logs/signals.log" gdbus monitor --session --dest "$SERVICE"
    if [ "${AUTO_ACCEPT:-0}" = 1 ]; then
        (
            tail -n0 -F "$logs/daemon.log" | grep --line-buffered "Asking for user consent" | while read -r line; do
                id=$(printf '%s' "$line" | sed -E 's/.*TransferMetadata \{ id: "([^"]*)".*/\1/')
                echo "$(date -u +%T) accepting $id"
                call Accept "$id"
            done
        ) > "$logs/auto-accept.log" 2>&1 &
        pids+=($!)
    fi
    if [ "${WITH_GUI:-0}" = 1 ] && [ -x "$DIR/kuickshare" ]; then
        XDG_CONFIG_HOME="$DIR/state/config" XDG_DATA_HOME="$DIR/state/data" bg "$logs/gui.log" "$DIR/kuickshare" --page transfers
    fi

    sleep "$DURATION" &
    wait $!
    stop_all
}

cmd_scan() {
    running || { echo "not running: test-laptop.sh start" >&2; exit 1; }
    local secs=${1:-20}
    call StartDiscovery >/dev/null 2>&1
    call ScanBluetooth >/dev/null 2>&1
    echo "looking for phones for ${secs}s: keep them on Quick Share's Receive screen (Everyone)"
    timeout "$secs" gdbus monitor --session --dest "$SERVICE" 2>/dev/null | grep --line-buffered "DeviceFound" \
        | sed -E "s/.*DeviceFound \(('[^']*'), \{.*'name': <['\"]([^'\"]*)['\"]>.*/\1  \2/; s/.*DeviceFound \(('[^']*'), \{.*'name': <\"([^\"]*)\">.*/\1  \2/" | sort -u
}

cmd_send() {
    running || { echo "not running: test-laptop.sh start" >&2; exit 1; }
    [ $# -eq 2 ] && [ -f "$2" ] || { echo "usage: test-laptop.sh send ID FILE" >&2; exit 1; }
    echo "$(date -u +%T) sending $(basename "$2") to $1; the ssh session may drop when the laptop joins the phone's network"
    echo "the active connection is $(nmcli -t -f NAME connection show --active | grep -v '^lo$' | head -1)"
    call SendFiles "$1" "['$(readlink -f "$2")']"
}

cmd_status() {
    running && echo "running (unit $UNIT)" || echo "not running"
    echo "logs: $(latest)"
    nmcli -t -f NAME,DEVICE,STATE connection show --active
    echo "leftover profiles: $(nmcli -t -f NAME connection show | grep -c '^kuickshare-')"
    ip -br -4 addr | grep -v '^lo'
}

cmd_report() {
    local l; l=$(latest); [ -n "$l" ] || { echo "no logs"; exit 1; }
    echo "== $l"
    grep -aE "sending over GATT|advertises no PSM|link up|receiver offers|join attempt|now on the hotspot|All files|canceled|lost the peer|ERROR|WARN" "$l/daemon.log" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-220
    echo "== active connections over time (changes only)"
    awk '/^==/{if (blk != prev) print stamp"\n"blk; prev=blk; blk=""; stamp=$0; next} /:/{blk=blk $0 "\n"} END{print stamp"\n"blk}' "$l/net-sampler.log" 2>/dev/null | head -80
    echo "== leftover profiles: $(nmcli -t -f NAME connection show | grep -c '^kuickshare-')"
}

cmd_stop() {
    systemctl --user stop "$UNIT.service" 2>&1
    echo "stopped; logs: $(latest)"
}

case "${1:-}" in
    start) shift; cmd_start "$@" ;;
    run) cmd_run ;;
    scan) shift; cmd_scan "$@" ;;
    send) shift; cmd_send "$@" ;;
    status) cmd_status ;;
    report) cmd_report ;;
    stop) cmd_stop ;;
    *) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
