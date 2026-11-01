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

    title: KI18n.i18nc("@title", "Preferences")

    // Same padding on every page: cards float with rounded corners instead of touching the window edges.
    topPadding: Kirigami.Units.largeSpacing
    leftPadding: Kirigami.Units.largeSpacing
    rightPadding: Kirigami.Units.largeSpacing
    bottomPadding: Kirigami.Units.largeSpacing

    PairDialog {
        id: pairDialog
        core: page.core
    }

    // Names the paired device the user asked to unpair, until the confirmation is answered.
    property string unpairName: ""
    property string unpairFingerprint: ""

    FolderDialog {
        id: folderDialog
        title: KI18n.i18nc("@title:window", "Choose Download Folder")
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

    // The switch and the description of "Check System Requirements…" follow the last report; the core answers from
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
        text: KI18n.i18nc("@title:group", "Visibility")
    }
    // FormCard rounds its corners only when it is wider than its maximumWidth (30 grid units by
    // default, wider than this window): cap every card just below its own width.
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormTextFieldDelegate {
            label: KI18n.i18nc("@label:textbox", "Device name")
            text: page.core.deviceName
            enabled: page.core.connected
            onEditingFinished: page.core.deviceName = text
        }
        FormCard.FormSwitchDelegate {
            icon.name: "visibility"
            text: KI18n.i18n("Visible to nearby devices")
            description: KI18n.i18n("Others can find this system and send to it")
            checked: page.core.visible
            enabled: page.core.connected
            onToggled: page.core.visible = checked
        }
        FormCard.FormSwitchDelegate {
            icon.name: "notifications-disabled"
            text: KI18n.i18nc("@option:check", "Hide during Do Not Disturb")
            description: KI18n.i18n("Nearby devices stop seeing this system while Do Not Disturb is on")
            checked: page.core.hideWhenDnd
            enabled: page.core.connected
            onToggled: page.core.hideWhenDnd = checked
        }
    }

    SectionTitle {
        text: KI18n.i18nc("@title:group", "Receiving")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormButtonDelegate {
            icon.name: "folder-download-symbolic"
            text: KI18n.i18nc("@action:button", "Choose Download Folder…")
            description: page.core.downloadDir.length > 0 ? page.core.downloadDir : KI18n.i18n("Default (Downloads)")
            enabled: page.core.connected
            onClicked: folderDialog.open()
        }
        FormCard.FormButtonDelegate {
            visible: page.core.downloadDir.length > 0
            icon.name: "folder-download"
            text: KI18n.i18nc("@action:button", "Use the Default Folder")
            onClicked: page.core.downloadDir = ""
        }
        FormCard.FormSwitchDelegate {
            id: randomPort
            icon.name: "network-wired-symbolic"
            text: KI18n.i18nc("@option:check", "Use a random port")
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
                    text: KI18n.i18nc("@label:spinbox", "Fixed port")
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
        text: KI18n.i18nc("@title:group", "Wi-Fi Direct")
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
            text: KI18n.i18nc("@option:check", "Receive and send without a shared network")
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
            text: KI18n.i18nc("@action:button", "Check System Requirements…")
            description: {
                const wr = wifiCard.wr;
                if (!page.core.connected || wr.error === "unavailable") {
                    return KI18n.i18n("KuickShare service unavailable");
                }
                if (wr.error === "schema") {
                    return KI18n.i18n("The result could not be read. Refresh to try again");
                }
                if (!wifiCard.hasReport) {
                    return KI18n.i18n("See what this system needs for Wi-Fi Direct");
                }
                if (wifiCard.blocked) {
                    return KI18n.i18n("Neither sending nor receiving can work on this system");
                }
                // "Works" only when the other direction was checked and can work.
                const works = s => s === "available" || s === "degraded";
                if (wifiCard.receiving === "unavailable" && works(wifiCard.sending)) {
                    return KI18n.i18n("Only sending works on this system");
                }
                if (wifiCard.sending === "unavailable" && works(wifiCard.receiving)) {
                    return KI18n.i18n("Only receiving works on this system");
                }
                const statuses = [wifiCard.sending, wifiCard.receiving, wr.capabilities.joinNetwork.status];
                if (statuses.every(s => s === "available")) {
                    // An informational row (dnsmasq in a Flatpak) feeds no capability, but was not checked either.
                    if (wr.components.some(c => c.status === "unknown")) {
                        return KI18n.i18n("Some items could not be checked");
                    }
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
    // Experimental and with side effects, so a warning box like the one under Trusted Devices.
    MessageBox {
        type: Kirigami.MessageType.Warning
        text: KI18n.i18n("Experimental. Needs NetworkManager, its dnsmasq package and Bluetooth. While on, this system is visible over Bluetooth to devices in range, and its Wi-Fi connection may drop while the temporary Wi-Fi link is active")
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
    }
    // A link, not a button in the box: URLs open from links (HIG getting_input#signaling-interactivity).
    Kirigami.UrlButton {
        url: "https://github.com/the-beper/KuickShare/blob/main/docs/wifi-direct.md"
        text: KI18n.i18nc("@label link to the documentation", "Wi-Fi Direct documentation")
        wrapMode: Text.Wrap
        // Left-aligned under the box (LinkButton centres its text), and only as wide as its text.
        horizontalAlignment: Text.AlignLeft
        Layout.maximumWidth: parent.width
        Layout.topMargin: Kirigami.Units.smallSpacing
    }

    SectionTitle {
        text: KI18n.i18nc("@title:group", "Paired Devices")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Repeater {
            model: page.core.trustedDevices
            delegate: FormCard.FormTextDelegate {
                id: paired

                required property var modelData

                // No leading icon: every item would show the same one (HIG icons#icons-for-list-items).
                text: modelData.name
                description: KI18n.i18nc("@info %1 is a shortened identifier of the device", "Device identifier: %1", modelData.fingerprint.substring(0, 12))
                trailing: QQC2.ToolButton {
                    icon.name: "edit-delete-remove"
                    display: QQC2.AbstractButton.IconOnly
                    text: KI18n.i18nc("@action:button %1 is a device name", "Unpair %1", paired.modelData.name)
                    onClicked: {
                        page.unpairName = paired.modelData.name;
                        page.unpairFingerprint = paired.modelData.fingerprint;
                        unpairDialog.open();
                    }
                }
            }
        }
        FormCard.FormTextDelegate {
            visible: page.core.trustedDevices.length === 0
            text: KI18n.i18n("No paired devices yet")
            description: KI18n.i18n("Paired devices can send to each other without asking")
        }
        FormCard.FormButtonDelegate {
            icon.name: "list-add"
            text: KI18n.i18nc("@action:button", "Pair a Device…")
            enabled: page.core.connected
            onClicked: pairDialog.open()
        }
    }
    MessageBox {
        type: Kirigami.MessageType.Information
        text: KI18n.i18n("Pairing works only between devices on the same local network")
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
    }

    SectionTitle {
        text: KI18n.i18nc("@title:group", "Trusted Devices")
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

                // No leading icon: every item would show the same one (HIG icons#icons-for-list-items).
                text: modelData.title
                description: KI18n.i18nc("@info %1 is the hardware address of the network adapter (MAC address)", "Hardware address: %1", modelData.mac)
                trailing: QQC2.ToolButton {
                    icon.name: "edit-delete-remove"
                    display: QQC2.AbstractButton.IconOnly
                    text: KI18n.i18nc("@action:button %1 is a device name", "Remove %1", allowed.modelData.title)
                    onClicked: page.core.forgetAllowed(allowed.modelData.name, allowed.modelData.mac)
                }
            }
        }
        FormCard.FormTextDelegate {
            visible: page.core.trustedDevicesEnabled && page.core.allowedDevices.length === 0
            text: KI18n.i18n("No devices yet")
            description: KI18n.i18n("Use “Accept and Trust” on an incoming transfer notification to add one")
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
        text: KI18n.i18nc("@title:group", "History")
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
                    model: [KI18n.i18nc("@item:inlistbox delete entries after", "Never"), KI18n.i18n("1 day"), KI18n.i18n("7 days"), KI18n.i18n("30 days"), KI18n.i18n("90 days")]
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
        text: KI18n.i18nc("@title:group", "Startup")
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
        text: KI18n.i18nc("@title:group", "Integration")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormSwitchDelegate {
            icon.name: "notifications-symbolic"
            text: KI18n.i18n("Keep running in the background")
            description: KI18n.i18n("Closing the window leaves KuickShare in the system tray, ready to receive. Its icon stands out during a transfer or a request")
            checked: page.core.trayIcon
            enabled: page.core.connected
            onToggled: page.core.trayIcon = checked
        }
        FormCard.FormSwitchDelegate {
            icon.name: "system-file-manager-symbolic"
            text: KI18n.i18nc("@option:check", "Show in Dolphin’s context menu")
            description: KI18n.i18n("Add “Send With KuickShare” to the menu of files")
            checked: page.core.dolphinIntegration
            enabled: page.core.connected
            onToggled: page.core.dolphinIntegration = checked
        }
        FormCard.FormSwitchDelegate {
            visible: page.core.appImage
            icon.name: "application-menu-symbolic"
            text: KI18n.i18nc("@option:check", "Show in the application menu")
            description: KI18n.i18n("Add KuickShare to the application menu")
            checked: page.core.menuEntry
            onToggled: page.core.menuEntry = checked
        }
    }
    MessageBox {
        visible: page.core.connected && !page.core.trayIcon
        type: Kirigami.MessageType.Information
        text: KI18n.i18n("Closing the window quits KuickShare, and nothing can be received until you open it again. At login it starts hidden, without a tray icon: open KuickShare again to show the window")
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
        subtitle: KI18n.i18n("firewalld is not running, so the firewall cannot be opened for a single port. If you agree, then until KuickShare is closed, the device that joins the hotspot can reach every service on this system")
        // The accepting button is named after what it does, so the footer is made of actions.
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            // The explicit consent, valid for this session only.
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Turn On")
                icon.name: "security-medium"
                onTriggered: {
                    page.core.requestWifiUpgrade(true, true);
                    firewallDialog.close();
                }
            },
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: firewallDialog.close()
            }
        ]
        // The switch was toggled by hand: show the real setting again.
        onClosed: wifiSwitch.checked = Qt.binding(() => page.core.wifiUpgrade)
    }
    Kirigami.PromptDialog {
        id: unpairDialog
        title: KI18n.i18n("Unpair %1?", page.unpairName)
        subtitle: KI18n.i18n("Transfers from this device will no longer be accepted without asking. Pair the devices again to undo this")
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Unpair")
                icon.name: "edit-delete-remove"
                onTriggered: {
                    page.core.unpair(page.unpairFingerprint);
                    unpairDialog.close();
                }
            },
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: unpairDialog.close()
            }
        ]
    }
    Kirigami.PromptDialog {
        id: resetDialog
        title: KI18n.i18n("Reset preferences?")
        subtitle: KI18n.i18n("Every setting goes back to its default. The history and the paired and trusted devices are kept")
        // The accepting button is named after what it does, so the footer is made of actions.
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Reset")
                icon.name: "document-revert"
                onTriggered: {
                    page.core.resetSettings();
                    resetDialog.close();
                }
            },
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: resetDialog.close()
            }
        ]
    }

    SectionTitle {
        text: KI18n.i18nc("@title:group", "Reset")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        FormCard.FormButtonDelegate {
            icon.name: "document-revert"
            text: KI18n.i18nc("@action:button", "Reset Preferences…")
            description: KI18n.i18n("Go back to the default preferences")
            enabled: page.core.connected
            onClicked: resetDialog.open()
        }
    }
}
