pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Dialogs
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.kde.ki18n

FormCard.FormCardPage {
    id: page

    required property var core

    title: KI18n.i18n("Preferences")

    // Same padding on every page: cards float with rounded corners instead of touching the window edges.
    topPadding: Kirigami.Units.largeSpacing
    leftPadding: Kirigami.Units.largeSpacing
    rightPadding: Kirigami.Units.largeSpacing
    bottomPadding: Kirigami.Units.largeSpacing

    PairDialog {
        id: pairDialog
        core: page.core
    }

    FolderDialog {
        id: folderDialog
        title: KI18n.i18n("Choose the download folder")
        onAccepted: page.core.downloadDir = page.core.toLocalFile(selectedFolder)
    }

    // Shows the port in use; while the port is random the box keeps the last fixed value.
    Binding {
        target: portBox
        property: "value"
        value: page.core.staticPort
        when: page.core.staticPort >= 1024
    }

    // Changing the port restarts the sharing service: wait until the user stops adjusting it.
    Timer {
        id: portTimer
        interval: 1000
        onTriggered: page.core.staticPort = portBox.value
    }

    WifiRequirementsDialog {
        id: requirementsDialog
        core: page.core
    }

    // The switch and the description of "Check system requirements…" follow the last report; the core answers from
    // its cache while it is fresh.
    Component.onCompleted: {
        if (page.core.connected) {
            page.core.loadWifiRequirements();
        }
    }
    Connections {
        target: page.core

        function onConnectedChanged() {
            if (page.core.connected) {
                page.core.loadWifiRequirements();
            }
        }
    }

    SectionTitle {
        first: true
        text: KI18n.i18n("Visibility")
    }
    // FormCard rounds its corners only when it is wider than its maximumWidth (30 grid units by
    // default, wider than this window): cap every card just below its own width.
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormTextFieldDelegate {
            label: KI18n.i18n("Device name")
            text: page.core.deviceName
            enabled: page.core.connected
            onEditingFinished: page.core.deviceName = text
        }
        FormCard.FormSwitchDelegate {
            icon.name: "visibility"
            text: KI18n.i18n("Visible to nearby devices")
            description: KI18n.i18n("Others can find this computer and send to it")
            checked: page.core.visible
            enabled: page.core.connected
            onToggled: page.core.visible = checked
        }
        FormCard.FormSwitchDelegate {
            icon.name: "notifications-disabled"
            text: KI18n.i18n("Hide during Do Not Disturb")
            description: KI18n.i18n("Not visible while Do Not Disturb is on")
            checked: page.core.hideWhenDnd
            enabled: page.core.connected
            onToggled: page.core.hideWhenDnd = checked
        }
    }

    SectionTitle {
        text: KI18n.i18n("Receiving")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormButtonDelegate {
            icon.name: "folder-download-symbolic"
            text: KI18n.i18n("Download folder")
            description: page.core.downloadDir.length > 0 ? page.core.downloadDir : KI18n.i18n("Default (Downloads)")
            enabled: page.core.connected
            onClicked: folderDialog.open()
        }
        FormCard.FormButtonDelegate {
            visible: page.core.downloadDir.length > 0
            icon.name: "edit-undo"
            text: KI18n.i18n("Use the default folder")
            onClicked: page.core.downloadDir = ""
        }
        FormCard.FormSwitchDelegate {
            id: randomPort
            icon.name: "network-wired-symbolic"
            text: KI18n.i18n("Random port")
            description: KI18n.i18n("A different port at every start")
            checked: page.core.staticPort === 0
            enabled: page.core.connected
            onToggled: page.core.staticPort = checked ? 0 : portBox.value
        }
        FormCard.AbstractFormDelegate {
            enabled: page.core.connected && !randomPort.checked
            background: null

            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing

                QQC2.Label {
                    text: KI18n.i18n("Fixed port")
                    Layout.fillWidth: true
                }
                QQC2.SpinBox {
                    id: portBox

                    Layout.preferredWidth: Kirigami.Units.gridUnit * 9
                    editable: true
                    from: 1024
                    to: 65535
                    onValueModified: portTimer.restart()
                }
            }
        }
    }
    FormCard.FormSectionText {
        Layout.bottomMargin: 0
        text: KI18n.i18n("A fixed port (1024–65535) can be opened in the firewall. Changing the name or the port restarts sharing.")
    }

    SectionTitle {
        text: KI18n.i18n("Wi-Fi Direct")
    }
    FormCard.FormCard {
        id: wifiCard

        readonly property var wr: page.core.wifiRequirements
        readonly property bool hasReport: wr.valid === true
        readonly property string sending: hasReport ? wr.capabilities.sending.status : ""
        readonly property string receiving: hasReport ? wr.capabilities.receiving.status : ""
        // The rule the core refuses the setting by: neither direction can work. A report that is only old still
        // counts (the core pushes changes), one followed by an error does not.
        readonly property bool blocked: !wr.error && sending === "unavailable" && receiving === "unavailable"

        maximumWidth: width - 1
        FormCard.FormSwitchDelegate {
            id: wifiSwitch
            icon.name: "network-wireless"
            text: KI18n.i18n("Receive and send without a shared network")
            description: KI18n.i18n("Be found over Bluetooth and move the transfer to a temporary Wi-Fi link")
            checked: page.core.wifiUpgrade
            // Off and greyed out while nothing can work; a setting that is already on can still be turned off.
            enabled: page.core.connected && (!wifiCard.blocked || page.core.wifiUpgrade)
            onToggled: {
                if (checked) {
                    page.core.enableWifiUpgrade();
                } else {
                    // The core also takes back the consent for this session, if it was given.
                    page.core.wifiUpgrade = false;
                }
                // Show only what the core reports: the switch moves when the setting really changes.
                wifiSwitch.checked = Qt.binding(() => page.core.wifiUpgrade);
            }
        }
        // Always reachable, also when the switch is disabled; the description sums up the last check.
        FormCard.FormButtonDelegate {
            icon.name: "view-list-details"
            text: KI18n.i18n("Check system requirements…")
            description: {
                const wr = wifiCard.wr;
                if (!page.core.connected || wr.error === "unavailable") {
                    return KI18n.i18n("KuickShare service unavailable");
                }
                if (wr.error === "schema") {
                    return KI18n.i18n("The result could not be read. Refresh to try again");
                }
                if (!wifiCard.hasReport) {
                    return KI18n.i18n("See what this computer needs for Wi-Fi Direct");
                }
                if (wifiCard.blocked) {
                    return KI18n.i18n("Neither sending nor receiving can work on this computer");
                }
                // "Works" only when the other direction was checked and can work.
                const works = s => s === "available" || s === "degraded";
                if (wifiCard.receiving === "unavailable" && works(wifiCard.sending)) {
                    return KI18n.i18n("Only sending works on this computer");
                }
                if (wifiCard.sending === "unavailable" && works(wifiCard.receiving)) {
                    return KI18n.i18n("Only receiving works on this computer");
                }
                const statuses = [wifiCard.sending, wifiCard.receiving, wr.capabilities.joinNetwork.status];
                if (statuses.every(s => s === "available")) {
                    return KI18n.i18n("Everything Wi-Fi Direct needs is available");
                }
                if (statuses.includes("unknown")) {
                    return KI18n.i18n("Some items could not be checked");
                }
                return KI18n.i18n("Some items need attention");
            }
            onClicked: requirementsDialog.open()
        }
    }
    // Experimental and with side effects, so a warning box like the one under Trusted devices.
    MessageBox {
        type: Kirigami.MessageType.Warning
        text: KI18n.i18n("Experimental. Needs NetworkManager, its dnsmasq package and Bluetooth. While on, this computer is visible over Bluetooth to devices in range, and its Wi-Fi connection may drop while the temporary Wi-Fi link is active. See the documentation")
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
    }

    SectionTitle {
        text: KI18n.i18n("Paired devices")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Repeater {
            model: page.core.trustedDevices
            delegate: FormCard.FormTextDelegate {
                id: paired

                required property var modelData

                icon.name: "security-high"
                text: modelData.name
                description: KI18n.i18n("Fingerprint %1", modelData.fingerprint.substring(0, 12))
                trailing: QQC2.ToolButton {
                    icon.name: "edit-delete-remove"
                    display: QQC2.AbstractButton.IconOnly
                    text: KI18n.i18n("Remove")
                    onClicked: page.core.unpair(paired.modelData.fingerprint)
                }
            }
        }
        FormCard.FormTextDelegate {
            visible: page.core.trustedDevices.length === 0
            text: KI18n.i18n("No paired computers yet")
            description: KI18n.i18n("Paired computers can send to each other without asking")
        }
        FormCard.FormButtonDelegate {
            icon.name: "list-add"
            text: KI18n.i18n("Pair a device…")
            enabled: page.core.connected
            onClicked: pairDialog.open()
        }
    }
    MessageBox {
        type: Kirigami.MessageType.Information
        text: KI18n.i18n("Pairing works only between computers on the same local network")
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
    }

    SectionTitle {
        text: KI18n.i18n("Trusted devices")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormSwitchDelegate {
            icon.name: "security-high"
            text: KI18n.i18n("Trust devices")
            description: KI18n.i18n("Accept transfers from trusted devices without asking")
            checked: page.core.trustedDevicesEnabled
            enabled: page.core.connected
            onToggled: page.core.trustedDevicesEnabled = checked
        }
        Repeater {
            model: page.core.trustedDevicesEnabled ? page.core.allowedDevices : []
            delegate: FormCard.FormTextDelegate {
                id: allowed

                required property var modelData

                icon.name: "smartphone-symbolic"
                text: modelData.title
                description: KI18n.i18n("MAC address %1", modelData.mac)
                trailing: QQC2.ToolButton {
                    icon.name: "edit-delete-remove"
                    display: QQC2.AbstractButton.IconOnly
                    text: KI18n.i18n("Remove")
                    onClicked: page.core.forgetAllowed(allowed.modelData.name, allowed.modelData.mac)
                }
            }
        }
        FormCard.FormTextDelegate {
            visible: page.core.trustedDevicesEnabled && page.core.allowedDevices.length === 0
            text: KI18n.i18n("No devices yet")
            description: KI18n.i18n("Use “Accept and trust” on an incoming transfer notification to add one")
        }
    }
    // A warning, not a hint, and only while trusting is on.
    MessageBox {
        visible: page.core.trustedDevicesEnabled
        type: Kirigami.MessageType.Warning
        text: KI18n.i18n("Devices are recognised by name and network address, which another device on the same network can copy: it would be accepted without asking too. Use only on a network where you trust every device")
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
    }

    SectionTitle {
        text: KI18n.i18n("History")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormSwitchDelegate {
            icon.name: "view-history"
            text: KI18n.i18n("Save transfer history")
            description: KI18n.i18n("Keep a list of sent and received transfers")
            checked: page.core.historyEnabled
            enabled: page.core.connected
            onToggled: page.core.historyEnabled = checked
        }
        FormCard.AbstractFormDelegate {
            id: keepRow

            readonly property var days: [0, 1, 7, 30, 90]

            enabled: page.core.connected && page.core.historyEnabled
            background: null

            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing

                Kirigami.Icon {
                    source: "edit-clear-history"
                    Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                    Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                }
                QQC2.Label {
                    text: KI18n.i18n("Delete entries after")
                    Layout.fillWidth: true
                }
                QQC2.ComboBox {
                    id: keepBox

                    Layout.preferredWidth: Kirigami.Units.gridUnit * 9
                    popup.width: width
                    model: [KI18n.i18n("Never"), KI18n.i18n("1 day"), KI18n.i18n("7 days"), KI18n.i18n("30 days"), KI18n.i18n("90 days")]
                    currentIndex: Math.max(0, keepRow.days.indexOf(page.core.historyKeepDays))
                    onActivated: index => page.core.historyKeepDays = keepRow.days[index]
                }
            }
        }
    }
    FormCard.FormSectionText {
        Layout.bottomMargin: 0
        text: KI18n.i18n("With the history turned off nothing is saved. Entries already saved stay until they expire or you clear them.")
    }

    SectionTitle {
        text: KI18n.i18n("Startup")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormSwitchDelegate {
            icon.name: "system-run-symbolic"
            text: KI18n.i18n("Start at login")
            description: KI18n.i18n("Starts hidden, in the system tray if it is shown")
            checked: page.core.autostart
            onToggled: page.core.autostart = checked
        }
    }
    SectionTitle {
        text: KI18n.i18n("Integration")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormSwitchDelegate {
            icon.name: "notifications-symbolic"
            text: KI18n.i18n("Show tray icon")
            description: KI18n.i18n("Keep KuickShare in the system tray")
            checked: page.core.trayIcon
            enabled: page.core.connected
            onToggled: page.core.trayIcon = checked
        }
        FormCard.FormSwitchDelegate {
            icon.name: "system-file-manager-symbolic"
            text: KI18n.i18n("Dolphin context menu")
            description: KI18n.i18n("Add “Send with KuickShare” to the menu of files")
            checked: page.core.dolphinIntegration
            enabled: page.core.connected
            onToggled: page.core.dolphinIntegration = checked
        }
        FormCard.FormSwitchDelegate {
            visible: page.core.appImage
            icon.name: "application-menu-symbolic"
            text: KI18n.i18n("Application menu entry")
            description: KI18n.i18n("Add KuickShare to the application menu")
            checked: page.core.menuEntry
            onToggled: page.core.menuEntry = checked
        }
    }
    MessageBox {
        visible: page.core.connected && !page.core.trayIcon
        type: Kirigami.MessageType.Information
        text: KI18n.i18n("Without the tray icon, closing the window quits KuickShare, and nothing can be received until you open it again. At login it starts hidden: open KuickShare again to show the window")
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
    }
    Connections {
        target: page.core

        function onFirewallWarning() {
            firewallDialog.open();
        }
    }
    Kirigami.PromptDialog {
        id: firewallDialog
        title: KI18n.i18n("Turn on without a firewall?")
        subtitle: KI18n.i18n("firewalld is not running, so the firewall cannot be opened for a single port. If you agree, then until KuickShare is closed, the phone that joins the hotspot can reach every service on this computer")
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        // The explicit consent, valid for this session only.
        onAccepted: page.core.requestWifiUpgrade(true, true)
        // The switch was toggled by hand: show the real setting again.
        onClosed: wifiSwitch.checked = Qt.binding(() => page.core.wifiUpgrade)
    }
    Kirigami.PromptDialog {
        id: resetDialog
        title: KI18n.i18n("Reset preferences?")
        subtitle: KI18n.i18n("Every setting goes back to its default. The history and the paired and trusted devices are kept")
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        onAccepted: page.core.resetSettings()
    }

    SectionTitle {
        text: KI18n.i18n("Reset")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormButtonDelegate {
            icon.name: "edit-undo"
            text: KI18n.i18n("Reset preferences…")
            description: KI18n.i18n("Go back to the default preferences")
            enabled: page.core.connected
            onClicked: resetDialog.open()
        }
    }
}
