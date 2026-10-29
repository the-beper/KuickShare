#!/usr/bin/env bash
# Hand test of the Wi-Fi upgrade and Bluetooth transfers: starts the built daemon and GUI and
# records everything into dist/logs/<timestamp>/ (symlink dist/logs/latest). The daemon and the
# GUI run with settings, history and received files in RAM (see sandbox.sh).
# Run it as your normal user, NOT with sudo: polkit refuses NetworkManager and firewalld to a
# process that is not in your active session. Only btmon is run through sudo (it asks once).
# Stop with Ctrl-C.   scripts/testing/test-wifi.sh
# Build first: build/build-env.sh cargo build && build/build-env.sh cmake --build dist/tmp/cmake-dev
set -u

[ "$(id -u)" -ne 0 ] || { echo "run as your normal user, without sudo" >&2; exit 1; }
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
GUI=$ROOT/dist/tmp/cmake-dev/kuickshare
DAEMON=$ROOT/target/debug/kuickshared
[ -x "$GUI" ] && [ -x "$DAEMON" ] || { echo "binaries missing, build first (see header)" >&2; exit 1; }

LOGS=$ROOT/dist/logs/$(date +%Y%m%d-%H%M%S)
mkdir -p "$LOGS" || { echo "cannot create $LOGS (owned by root? sudo chown -R $USER: $ROOT/dist/logs)" >&2; exit 1; }
ln -sfn "$LOGS" "$ROOT/dist/logs/latest"

sudo -v || exit 1

PIDS=()
bg() { # bg <logfile> <command...>
    local out=$1; shift
    "$@" > "$out" 2>&1 &
    PIDS+=($!)
}
cleanup() {
    echo "stopping..."
    kill "${PIDS[@]}" 2>/dev/null
    sudo pkill -x btmon 2>/dev/null
    sleep 1
    kill -9 "${PIDS[@]}" 2>/dev/null
    sandbox_cleanup
    echo "logs: $LOGS"
}
# shellcheck source=sandbox.sh
. "$(dirname "$0")/sandbox.sh"
trap cleanup EXIT
trap 'exit 0' INT TERM HUP

# Snapshot of the state a run starts from
{
    date
    echo "--- bluetoothctl show"; bluetoothctl show
    echo "--- nmcli device"; nmcli device status
    echo "--- nmcli connection"; nmcli connection show
    echo "--- firewalld"; firewall-cmd --state 2>&1; firewall-cmd --get-active-zones 2>&1
    echo "--- dnsmasq"; command -v dnsmasq
    echo "--- rfkill"; rfkill list
    echo "--- session"; loginctl session-status 2>&1 | head -8
} > "$LOGS/state-before.txt" 2>&1

# System side
bg "$LOGS/btmon.txt" sudo btmon -t
bg "$LOGS/btmon-raw.log" sudo btmon -w "$LOGS/capture.btsnoop"
bg "$LOGS/system.log" journalctl -f -o short-precise \
    -u NetworkManager -u bluetooth -u firewalld -u wpa_supplicant
bg "$LOGS/kernel.log" journalctl -k -f -o short-precise
bg "$LOGS/nm-monitor.log" nmcli monitor

# Phones: one logcat per adb device
for serial in $(adb devices | awk 'NR>1 && $2=="device"{print $1}'); do
    adb -s "$serial" logcat -c
    bg "$LOGS/logcat-$serial.log" adb -s "$serial" logcat -v threadtime \
        -s NearbyConnections NearbyMediums NearbySharing WifiP2pService
    adb -s "$serial" shell getprop ro.product.model > "$LOGS/phone-$serial.txt" 2>&1
done

# NAT and firewall state of the shared connection: a sample is written only when it changes,
# so nat.log shows the rules NetworkManager adds when the hotspot starts and removes at the end
nat_state() {
    echo "ip_forward: $(sysctl -n net.ipv4.ip_forward)"
    echo "--- nft"; sudo nft list ruleset 2>&1
    echo "--- iptables nat"; sudo iptables-save -t nat 2>&1
    echo "--- firewalld"; firewall-cmd --get-active-zones 2>&1
    echo "--- addresses"; ip -br -4 addr
}
(
    prev=
    while :; do
        sudo -v # keep the credentials fresh, a run can outlast the 15 minute cache
        cur=$(nat_state)
        if [ "$cur" != "$prev" ]; then
            echo "===== $(date -u +%T)"; echo "$cur"; prev=$cur
        fi
        sleep 2
    done
) > "$LOGS/nat.log" 2>&1 &
PIDS+=($!)

# Plasma job tracker: the calls the shell's transfer job makes on the session bus
# (description, speed, percent, capabilities, terminate). Compare with what the
# notification / job popup showed.
bg "$LOGS/jobview.log" dbus-monitor --session \
    "interface='org.kde.JobViewServer'" "interface='org.kde.JobViewV2'" \
    "interface='org.kde.JobViewV3'" "interface='org.kde.kuiserver'"

# Daemon first so its log is ours; the GUI then finds it on the session bus
RUST_LOG=info,rqs_lib::hdl::ble_receiver=debug,rqs_lib::hdl::ble_scan=debug,rqs_lib::hdl::ble_send=debug,rqs_lib::upgrade=debug,kuickshared::link=debug,kuickshared::app=debug \
    bg "$LOGS/daemon.log" env "${SANDBOX_ENV[@]}" "$DAEMON"
sleep 2
bg "$LOGS/gui.log" env "${SANDBOX_ENV[@]}" "$GUI" --page settings

echo "recording in $LOGS (Ctrl-C to stop)"
echo "turn on Settings -> Wi-Fi Direct in the GUI, then run the transfer"
wait
