#!/usr/bin/env python3
"""Stand-in for kuickshared with made-up data, for screenshots. Run on a private session bus only
(demo.sh does that); on the real session bus it would try to take the name of the real core."""
import json, time
from gi.repository import Gio, GLib

NAME = "io.github.the_beper.KuickShare"
PATH = "/io/github/the_beper/KuickShare"
IFACE = "io.github.the_beper.KuickShare1"

XML = """<node><interface name="@IFACE@">
<method name="Accept"><arg type="s" direction="in"/></method>
<method name="Decline"><arg type="s" direction="in"/></method>
<method name="Cancel"><arg type="s" direction="in"/></method>
<method name="StartDiscovery"/><method name="StopDiscovery"/>
<method name="SendFiles"><arg type="s" direction="in"/><arg type="as" direction="in"/></method>
<method name="SendText"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/></method>
<method name="History"><arg type="s" direction="out"/></method>
<method name="ClearHistory"/>
<method name="HistorySize"><arg type="t" direction="out"/></method>
<method name="RemoveHistory"><arg type="t" direction="in"/></method>
<method name="SetPairing"><arg type="b" direction="in"/></method>
<method name="PairWith"><arg type="s" direction="in"/></method>
<method name="ConfirmPairing"><arg type="b" direction="in"/></method>
<method name="Unpair"><arg type="s" direction="in"/></method>
<method name="TrustedDevices"><arg type="s" direction="out"/></method>
<method name="AllowDevice"><arg type="s" direction="in"/></method>
<method name="ForgetAllowed"><arg type="s" direction="in"/><arg type="s" direction="in"/></method>
<method name="AllowedDevices"><arg type="s" direction="out"/></method>
<method name="ResetSettings"/>
<property name="DeviceName" type="s" access="readwrite"/>
<property name="DownloadDir" type="s" access="readwrite"/>
<property name="StaticPort" type="u" access="readwrite"/>
<property name="Visible" type="b" access="readwrite"/>
<property name="HistoryEnabled" type="b" access="readwrite"/>
<property name="HistoryKeepDays" type="u" access="readwrite"/>
<property name="TrustedDevicesEnabled" type="b" access="readwrite"/>
<property name="HideHistoryFilters" type="b" access="readwrite"/>
<property name="TrayIcon" type="b" access="readwrite"/>
<property name="DolphinIntegration" type="b" access="readwrite"/>
<property name="HideWhenDnd" type="b" access="readwrite"/>
<property name="Suspended" type="b" access="readwrite"/>
<signal name="TransferUpdated"><arg type="s"/><arg type="a{sv}"/></signal>
<signal name="DeviceFound"><arg type="s"/><arg type="a{sv}"/></signal>
<signal name="DeviceLost"><arg type="s"/></signal>
<signal name="PairingCode"><arg type="s"/><arg type="s"/></signal>
<signal name="PairingFinished"><arg type="b"/><arg type="s"/><arg type="s"/></signal>
<signal name="TrustedChanged"/>
</interface></node>""".replace("@IFACE@", IFACE)

V = GLib.Variant
props = {
    "DeviceName": V("s", "TuxDesktop"), "DownloadDir": V("s", "/home/tux/Downloads"),
    "StaticPort": V("u", 0), "Visible": V("b", True), "HistoryEnabled": V("b", True),
    "HistoryKeepDays": V("u", 30), "TrustedDevicesEnabled": V("b", True), "TrayIcon": V("b", True),
    "DolphinIntegration": V("b", True), "HideWhenDnd": V("b", True), "Suspended": V("b", False),
    "HideHistoryFilters": V("b", False),
}

DEVICES = [
    ("dev-pixel", "Tux Phone", "192.168.1.20", "Phone", "trusted"),
    ("dev-tab", "Tux Tablet", "192.168.1.31", "Tablet", ""),
    ("dev-lap", "Tux Laptop", "192.168.1.44", "Laptop", ""),
    ("dev-long", "Tux Chromebook in the living room with a very long name", "192.168.1.58", "Laptop", ""),
]

now = int(time.time())
HISTORY = [
    dict(uid=(now - 300) * 1000, time=now - 300, kind="inbound", state="Finished", device="Tux Phone",
         payload_kind="Files", files=["IMG_2041.jpg", "IMG_2042.jpg", "IMG_2043.jpg"], text="", total_bytes=9_400_000, trusted=True),
    dict(uid=(now - 2400) * 1000, time=now - 2400, kind="outbound", state="Finished", device="Tux Laptop",
         payload_kind="Url", files=[], text="https://kde.org/plasma-desktop/", total_bytes=0, trusted=False),
    dict(uid=(now - 7200) * 1000, time=now - 7200, kind="inbound", state="Finished", device="Tux Tablet",
         payload_kind="Files", files=["Quarterly report.pdf"], text="", total_bytes=2_100_000, trusted=False),
    dict(uid=(now - 90000) * 1000, time=now - 90000, kind="outbound", state="Rejected", device="Tux Tablet",
         payload_kind="Files", files=["holiday-video.mp4"], text="", total_bytes=412_000_000, trusted=False),
    dict(uid=(now - 200000) * 1000, time=now - 200000, kind="inbound", state="Finished", device="Tux Phone",
         payload_kind="Text", files=[], text="Meet me at the station at 6, platform 2", total_bytes=0, trusted=True),
]
PEERS = [dict(name="Tux Phone", fingerprint="ab12" * 16, public_key="00" * 32, paired_at=now - 86400 * 20)]
ALLOWED = [dict(name="Tux Chromebook in the living room with a very long name", mac="aa:bb:cc:dd:ee:ff")]

conn = None


def emit(name, params):
    conn.emit_signal(None, PATH, IFACE, name, params)


def info(**kw):
    d = dict(kind="", state="", device="", pin="", preview="", payload_kind="Files", files="", text="",
             trusted="", always_accepted="")
    d.update({k: v for k, v in kw.items() if isinstance(v, str)})
    out = {k: V("s", v) for k, v in d.items()}
    out["total_bytes"] = V("t", kw.get("total_bytes", 0))
    out["ack_bytes"] = V("t", kw.get("ack_bytes", 0))
    out["history_uid"] = V("t", 0)
    return out


def send_devices():
    for did, name, ip, kind, trust in DEVICES:
        emit("DeviceFound", V("(sa{sv})", (did, {"name": V("s", name), "ip": V("s", ip), "port": V("s", "5200"),
                                               "device_type": V("s", kind), "trust": V("s", trust)})))


def send_transfers():
    for tid, kw in [
        ("t1", dict(kind="inbound", state="WaitingForUserConsent", device="Tux Tablet", pin="4821",
                    files="Holiday photos.zip\nitinerary.pdf", total_bytes=48_300_000)),
        ("t2", dict(kind="outbound", state="SendingFiles", device="Tux Laptop", files="/home/tux/Documents/slides.odp",
                    total_bytes=24_000_000, ack_bytes=15_000_000)),
    ]:
        emit("TransferUpdated", V("(sa{sv})", (tid, info(**kw))))


seen = {"history": False}


def on_call(c, sender, path, iface, method, params, inv):
    if method == "History":
        inv.return_value(V("(s)", (json.dumps(HISTORY),)))
        if not seen["history"]:
            seen["history"] = True
            GLib.timeout_add(700, lambda: send_transfers() or False)
        return
    if method == "StartDiscovery":
        GLib.timeout_add(300, lambda: send_devices() or False)
    out = {"HistorySize": V("(t)", (4_200_000,)), "TrustedDevices": V("(s)", (json.dumps(PEERS),)),
           "AllowedDevices": V("(s)", (json.dumps(ALLOWED),))}.get(method)
    inv.return_value(out)


def on_get(c, sender, path, iface, name):
    return props[name]


def on_set(c, sender, path, iface, name, value):
    props[name] = value
    return True


def on_bus(c, name):
    global conn
    conn = c
    node = Gio.DBusNodeInfo.new_for_xml(XML)
    c.register_object(PATH, node.interfaces[0], on_call, on_get, on_set)


loop = GLib.MainLoop()
Gio.bus_own_name(Gio.BusType.SESSION, NAME, Gio.BusNameOwnerFlags.NONE, on_bus, None, lambda *a: loop.quit())
loop.run()
