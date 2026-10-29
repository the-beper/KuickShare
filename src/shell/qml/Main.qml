pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Window
import QtQuick.Controls as QQC2
import QtQuick.Templates as T
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.ki18n

Kirigami.ApplicationWindow {
    id: root

    required property var core
    property string startPage: "send"
    // The AppImage asks once whether to add itself to the menus (decided in C++).
    property bool askIntegration: false

    property string currentPage: ""
    // Files queued for sending; filled from the Send page or by dropping onto the window.
    property var outgoingFiles: []
    // The send in progress: when it finishes the Send page is left clean, ready for the next one.
    // A failed send keeps the files and the text so that it can be tried again.
    property string pendingSendId: ""
    // Why the firewall stopped the hotspot last (a code of the core).
    property string wifiBlockedReason: ""

    title: KI18n.i18n("KuickShare")
    // Compact, fixed size (so it cannot be maximized or resized): the pages scroll when needed.
    readonly property real fixedWidth: Kirigami.Units.gridUnit * 26
    readonly property real fixedHeight: Math.min(Kirigami.Units.gridUnit * 37, Screen.desktopAvailableHeight * 0.9)
    width: fixedWidth
    height: fixedHeight
    minimumWidth: fixedWidth
    maximumWidth: fixedWidth
    minimumHeight: fixedHeight
    maximumHeight: fixedHeight
    flags: Qt.Window | Qt.WindowTitleHint | Qt.WindowSystemMenuHint | Qt.WindowMinimizeButtonHint | Qt.WindowCloseButtonHint
    // The window is shown from C++ unless the app was started with --background.
    visible: false

    Connections {
        target: root.core

        function onTransferChanged(id) {
            if (id !== root.pendingSendId) {
                return;
            }
            const state = root.core.transfers.info(id).state;
            if (state !== "Finished") {
                // A failed send keeps its files and text for another try; no state means the row is gone.
                if (state === undefined || ["Cancelled", "Rejected", "Disconnected"].includes(state)) {
                    root.pendingSendId = "";
                }
                return;
            }
            root.pendingSendId = "";
            root.outgoingFiles = [];
            (root.pageStack.currentItem as SendPage)?.clearText();
        }
    }

    function show(name) {
        if (name === currentPage) {
            return;
        }
        currentPage = name;
        pageStack.clear();
        const page = pageStack.push(name === "help" ? helpPage : name === "settings" ? settingsPage : name === "transfers" ? transfersPage : sendPage);
        if (page) {
            pageFade.target = page;
            pageFade.restart();
        }
    }

    // The new page fades in with the easing the rest of the app uses; the stack's own slide is off, as it fought the fade.
    NumberAnimation {
        id: pageFade
        property: "opacity"
        from: 0
        to: 1
        duration: Kirigami.Units.longDuration
        easing.type: Easing.OutCubic
    }

    function openFiles() {
        show("send");
        Qt.callLater(() => (root.pageStack.currentItem as SendPage)?.openFileDialog());
    }
    function pasteClipboard() {
        show("send");
        Qt.callLater(() => (root.pageStack.currentItem as SendPage)?.pasteFromClipboard());
    }
    // Shows the details of a transfer (the "View" action of a notification).
    function showTransfer(id) {
        startPage = "transfers";
        show("transfers");
        Qt.callLater(() => (root.pageStack.currentItem as TransfersPage)?.showDetailsFor(id));
    }
    // The `--device` option: the same send as a click on the device card of the Send page.
    // `files` is what the request asked for; it replaces the queue so that nothing else joins the send.
    function sendQueuedTo(deviceId, files) {
        outgoingFiles = files;
        show("send");
        (root.pageStack.currentItem as SendPage)?.sendTo(deviceId);
    }
    function showError(text) {
        show("send");
        toasts.show(Kirigami.MessageType.Error, text, 10000);
    }
    function showSettings() {
        show("settings");
    }

    // Quitting stops the sharing service: ask first if something is being sent or received.
    function requestQuit() {
        if (core.transferActive()) {
            quitDialog.open();
        } else {
            Qt.quit();
        }
    }

    Kirigami.PromptDialog {
        id: quitDialog
        title: KI18n.i18n("Quit KuickShare?")
        subtitle: KI18n.i18n("A transfer is in progress. Quitting stops it")
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        onAccepted: Qt.quit()
    }

    function queueFiles(paths) {
        const files = outgoingFiles.slice();
        for (const path of paths) {
            if (files.indexOf(path) < 0) {
                files.push(path);
            }
        }
        outgoingFiles = files;
        show("send");
    }

    function addFiles(urls) {
        const added = core.filesFromUrls(urls);
        const files = outgoingFiles.slice();
        for (const path of added) {
            if (files.indexOf(path) < 0) {
                files.push(path);
            }
        }
        outgoingFiles = files;
    }

    // Closing the window keeps the app running in the tray.
    // Without the tray icon there is no way back to a hidden window, so closing it quits the app.
    onClosing: close => {
        close.accepted = false;
        if (core.trayIcon) {
            root.hide();
        } else {
            root.requestQuit();
        }
    }

    Connections {
        target: root.core
        function onErrorOccurred(message) {
            toasts.show(Kirigami.MessageType.Error, message, 10000);
        }
        // The firewall did not allow the computer's hotspot: the transfer stays on Bluetooth.
        function onWifiUpgradeBlocked(reason) {
            root.wifiBlockedReason = reason;
            wifiBlockedBox.visible = true;
        }
        function onNetworkJoined(ssid) {
            toasts.show(Kirigami.MessageType.Positive, KI18n.i18n("Connected to %1", ssid), 6000);
        }
        // How a transfer ended, as a toast on whatever page is open: green for success, red for a
        // failure, blue otherwise.
        function onTransferEnded(id, state) {
            switch (state) {
            case "Finished":
                toasts.show(Kirigami.MessageType.Positive, KI18n.i18n("Transfer completed"), 4000);
                break;
            case "Disconnected":
                toasts.show(Kirigami.MessageType.Error, KI18n.i18n("The transfer failed"), 10000);
                break;
            case "Rejected":
                toasts.show(Kirigami.MessageType.Information, KI18n.i18n("The transfer was declined"), 6000);
                break;
            default:
                toasts.show(Kirigami.MessageType.Information, KI18n.i18n("The transfer was cancelled"), 6000);
            }
        }
    }

    pageStack.globalToolBar.style: Kirigami.ApplicationHeaderStyle.None
    pageStack.columnView.scrollDuration: 0

    header: Item {
        implicitHeight: tabs.implicitHeight

        // The bar spans the whole header, so its own line runs under "More" as well; the room for
        // the button is taken from its padding.
        Kirigami.NavigationTabBar {
            id: tabs

            anchors.fill: parent
            // The stock bar centres its tabs with extra side padding; here they start at the edge, and the
            // room at the right is the button plus a small gap.
            leftPadding: 0
            rightPadding: more.width + Kirigami.Units.largeSpacing

            delegate: Kirigami.NavigationTabButton {
                id: tab

                required property T.Action modelData

                parent: tabs.contentItem
                action: modelData
                // The same workaround as the stock delegate: without it the tabs are not checkable.
                onActionChanged: action.checkable = true
                display: T.AbstractButton.TextBesideIcon

                Layout.minimumWidth: tabs.buttonWidth
                Layout.maximumWidth: tabs.buttonWidth
                Layout.fillHeight: true

                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing

                    Item {
                        Layout.fillWidth: true
                    }
                    Kirigami.Icon {
                        source: tab.icon.name
                        color: tab.icon.color
                        Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                        Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                    }
                    QQC2.Label {
                        text: tab.text
                        color: Kirigami.Theme.textColor
                    }
                    Item {
                        Layout.fillWidth: true
                    }
                }
            }

            actions: [
                Kirigami.Action {
                    text: KI18n.i18n("Send")
                    icon.name: "document-send"
                    checked: root.currentPage === "send"
                    shortcut: "Ctrl+1"
                    onTriggered: root.show("send")
                },
                Kirigami.Action {
                    text: KI18n.i18n("Transfers")
                    icon.name: "folder-sync"
                    checked: root.currentPage === "transfers"
                    shortcut: "Ctrl+2"
                    onTriggered: root.show("transfers")
                }
            ]
        }
        // A short vertical line midway between the visible edge of the last tab and the button; it stays
        // clear of the line under the header.
        Kirigami.Separator {
            x: Math.round(((tabs.width - tabs.rightPadding - Kirigami.Units.smallSpacing) + more.x) / 2)
            height: Math.round(parent.height * 0.55)
            anchors.verticalCenter: parent.verticalCenter
        }
        QQC2.ToolButton {
            id: more

            text: KI18n.i18n("More")
            icon.name: "open-menu-symbolic"
            // Only the icon; the text is for screen readers.
            display: QQC2.AbstractButton.IconOnly
            down: root.currentPage === "settings" || root.currentPage === "help"
            anchors {
                right: parent.right
                rightMargin: Kirigami.Units.smallSpacing + tabs.leftPadding
                verticalCenter: parent.verticalCenter
            }
            onClicked: moreMenu.open()
        }
    }

    Kirigami.Action {
        id: settingsAction
        text: KI18n.i18n("Preferences")
        icon.name: "configure"
        shortcut: "Ctrl+,"
        onTriggered: root.show("settings")
    }
    Kirigami.Action {
        id: shortcutsAction
        text: KI18n.i18n("Shortcuts")
        icon.name: "input-keyboard-symbolic"
        shortcut: "Ctrl+?"
        onTriggered: shortcutsDialog.open()
    }
    Kirigami.Action {
        id: quitAction
        text: KI18n.i18n("Quit")
        icon.name: "application-exit"
        shortcut: StandardKey.Quit
        onTriggered: root.requestQuit()
    }
    Kirigami.Action {
        id: helpAction
        text: KI18n.i18n("Help")
        icon.name: "help-contents"
        shortcut: "Ctrl+H"
        onTriggered: root.show("help")
    }
    Kirigami.Action {
        id: aboutAction
        text: KI18n.i18n("About")
        icon.name: "help-about-symbolic"
        onTriggered: aboutDialog.open()
    }
    QQC2.Menu {
        id: moreMenu

        parent: root.contentItem
        x: parent.width - width - Kirigami.Units.largeSpacing
        y: Kirigami.Units.smallSpacing

        QQC2.MenuItem {
            action: settingsAction
        }
        QQC2.MenuItem {
            action: shortcutsAction
        }
        QQC2.MenuItem {
            action: helpAction
        }
        QQC2.MenuItem {
            action: aboutAction
        }
        QQC2.MenuSeparator {
        }
        QQC2.MenuItem {
            action: quitAction
        }
    }

    Component {
        id: sendPage
        SendPage {
            core: root.core
            files: root.outgoingFiles
            dragging: dropArea.containsDrag
            onFilesAdded: urls => root.addFiles(urls)
            onFileRemoved: path => root.outgoingFiles = root.outgoingFiles.filter(p => p !== path)
            onFilesCleared: root.outgoingFiles = []
            onSent: deviceId => {
                root.pendingSendId = deviceId;
                root.show("transfers");
            }
        }
    }
    Component {
        id: transfersPage
        TransfersPage {
            core: root.core
            toasts: toasts
        }
    }
    Component {
        id: settingsPage
        SettingsPage {
            core: root.core
        }
    }
    ShortcutsDialog {
        id: shortcutsDialog
    }
    IntegrationDialog {
        id: integrationDialog
        core: root.core
    }
    AboutDialog {
        id: aboutDialog
        core: root.core
    }

    Component {
        id: helpPage
        HelpPage {
            core: root.core
        }
    }

    // Keyboard shortcuts (the tabs have theirs on the actions above). They are listed in Help.
    Shortcut {
        sequence: "Ctrl+Shift+/"
        onActivated: shortcutsDialog.open()
    }
    Shortcut {
        sequence: "F1"
        onActivated: root.show("help")
    }
    Shortcut {
        sequence: StandardKey.Open
        onActivated: {
            root.show("send");
            // The page may have just been created: ask it once it exists.
            Qt.callLater(() => (root.pageStack.currentItem as SendPage)?.openFileDialog());
        }
    }
    // On the Send page, pasting anywhere (outside the text box, which has its own) adds what is on the clipboard.
    Shortcut {
        sequences: [StandardKey.Paste]
        enabled: root.currentPage === "send"
        onActivated: root.pasteClipboard()
    }
    Shortcut {
        sequence: "Ctrl+L"
        onActivated: {
            root.outgoingFiles = [];
            (root.pageStack.currentItem as SendPage)?.clearText();
        }
    }
    Shortcut {
        sequences: [StandardKey.Close]
        onActivated: root.close()
    }

    ToastStack {
        id: toasts
    }

    // The sharing service is not running (it stopped, or never started): offer to start it.
    MessageBox {
        parent: root.contentItem
        z: 10
        anchors {
            top: parent.top
            left: parent.left
            right: parent.right
            margins: Kirigami.Units.largeSpacing
        }
        visible: !root.core.connected && !root.core.serviceStarting
        type: Kirigami.MessageType.Error
        text: KI18n.i18n("The sharing service is not running. Nothing can be sent or received until it is started")
        actions: [
            Kirigami.Action {
                text: KI18n.i18n("Start service")
                icon.name: "media-playback-start"
                onTriggered: root.core.startService()
            }
        ]
    }

    // A newer version was installed while this one runs (Flatpak, AppImage).
    MessageBox {
        parent: root.contentItem
        z: 10
        anchors {
            top: parent.top
            left: parent.left
            right: parent.right
            margins: Kirigami.Units.largeSpacing
        }
        visible: root.core.updateInstalled && root.core.connected
        type: Kirigami.MessageType.Information
        text: KI18n.i18n("A new version of KuickShare is installed")
        actions: [
            Kirigami.Action {
                text: KI18n.i18n("Restart")
                icon.name: "view-refresh"
                enabled: !root.core.transferBusy
                onTriggered: root.core.restartForUpdate()
            }
        ]
    }

    // Once, after an update: the trusted devices of the older version were removed (the core keeps the
    // notice until it is acknowledged, so a restart before the user saw it does not lose it).
    MessageBox {
        parent: root.contentItem
        z: 10
        anchors {
            top: parent.top
            left: parent.left
            right: parent.right
            margins: Kirigami.Units.largeSpacing
        }
        visible: root.core.trustedDevicesReset && root.core.connected
        type: Kirigami.MessageType.Information
        text: KI18n.i18n("Trusted devices were removed: they were recognised only by name and network address, which another device on the same network can copy. Trust a device again from its next transfer")
        actions: [
            Kirigami.Action {
                text: KI18n.i18n("Got it")
                icon.name: "dialog-ok-apply"
                onTriggered: root.core.acknowledgeTrustedDevicesReset()
            }
        ]
    }

    // The firewall refused the hotspot: say so, and offer the consent for this session (it is the core
    // that decides and remembers it). Shown by `onWifiUpgradeBlocked`; the close button hides it.
    MessageBox {
        id: wifiBlockedBox
        parent: root.contentItem
        z: 10
        anchors {
            top: parent.top
            left: parent.left
            right: parent.right
            margins: Kirigami.Units.largeSpacing
        }
        visible: false
        showCloseButton: true
        type: Kirigami.MessageType.Warning
        // An unknown code is taken as a refusal.
        text: root.wifiBlockedReason === "firewall_missing" ? KI18n.i18n("No firewall service that KuickShare can open a port in was found. The transfer continues over Bluetooth, which is very slow") : KI18n.i18n("The firewall did not allow the Wi-Fi link. The transfer continues over Bluetooth, which is very slow")
        actions: [
            Kirigami.Action {
                text: KI18n.i18n("Allow for this session…")
                icon.name: "security-medium"
                enabled: root.core.connected
                onTriggered: wifiConsentDialog.open()
            }
        ]
    }
    Kirigami.PromptDialog {
        id: wifiConsentDialog
        title: KI18n.i18n("Allow the hotspot without a firewall rule?")
        subtitle: KI18n.i18n("If you agree, then until KuickShare is closed, the phone that joins the hotspot can reach every service on this computer. It applies to the next transfer, not to the one in progress")
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        // The explicit consent, valid for this session only.
        onAccepted: {
            // Turned off meanwhile: the consent must not turn it on again.
            if (root.core.wifiUpgrade) {
                root.core.requestWifiUpgrade(true, true);
            }
            wifiBlockedBox.visible = false;
        }
    }

    DropArea {
        id: dropArea
        anchors.fill: parent
        onDropped: drop => {
            if (drop.hasUrls) {
                root.addFiles(drop.urls);
                root.show("send");
            }
        }
    }

    // Asked once the service is up, because "Not now" changes one of its settings.
    Connections {
        target: root.core
        enabled: root.askIntegration
        function onConnectedChanged() {
            if (root.core.connected) {
                root.askIntegration = false;
                integrationDialog.open();
            }
        }
    }

    Component.onCompleted: {
        // A page shown right after creation (tray actions) wins over the start page.
        Qt.callLater(() => {
            if (currentPage === "") {
                show(startPage);
            }
        });
        if (askIntegration && core.connected) {
            askIntegration = false;
            integrationDialog.open();
        }
    }
}
