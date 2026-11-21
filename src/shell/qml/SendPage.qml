pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Dialogs
import QtQuick.Layouts
import QtQuick.Window
import org.kde.kirigami as Kirigami
import org.kde.ki18n

Kirigami.ScrollablePage {
    id: page

    required property var core
    required property var files
    property bool dragging: false
    // The content of the text box. Main owns it, so the text survives a change of page.
    property string draft: ""

    signal draftEdited(string text)
    signal showHelpRequested
    signal showTransfersRequested
    signal filesAdded(var urls)
    signal fileRemoved(string path)
    signal filesCleared
    signal sent(string deviceId)

    readonly property var details: files.map(path => core.fileDetails(path))
    readonly property real totalBytes: details.reduce((sum, file) => sum + file.bytes, 0)
    // What is sent: no trailing spaces on any line, and no spaces or line breaks around the text.
    readonly property string trimmedText: textField.text.replace(/\r/g, "").replace(/[ \t]+$/gm, "").trim()
    // The longest text that can be sent; the counter shows up when it is close.
    readonly property int maxChars: 100000
    readonly property bool nearLimit: textField.length >= maxChars * 0.9
    readonly property bool atLimit: textField.length >= maxChars
    readonly property bool hasContent: files.length > 0 || trimmedText.length > 0
    // The protocol has a link type for exactly one link; anything more goes as text.
    // A link is at least protocol://word.c, with any protocol; anything shorter is not a valid link yet.
    readonly property bool startsWithLink: /^[a-z][a-z0-9+.-]*:\/\/\S+\.\S/i.test(trimmedText)
    readonly property bool singleLink: /^[a-z][a-z0-9+.-]*:\/\/\S+\.\S+$/i.test(trimmedText)
    // The user's choice for a single link: send it as text instead of as a link.
    property bool sendAsText: false
    onSingleLinkChanged: sendAsText = false

    title: KI18n.i18nc("@title", "Send")
    // Same padding on every page: cards float with rounded corners instead of touching the window edges.
    topPadding: Kirigami.Units.largeSpacing
    leftPadding: Kirigami.Units.largeSpacing
    rightPadding: Kirigami.Units.largeSpacing
    bottomPadding: Kirigami.Units.largeSpacing

    // Discovery runs only while this page is open in a visible window; the core keeps it alive
    // for a short while after that, so quick tab switches don't restart it.
    readonly property bool wanted: Window.window ? Window.window.visible : false
    onWantedChanged: {
        searchTimedOut = false;
        wanted ? core.startDiscovery() : core.stopDiscovery();
    }
    onDraftChanged: if (textField.text !== draft) textField.text = draft

    // Set after a while of discovery without any device.
    property bool searchTimedOut: false
    Timer {
        interval: 20000
        running: page.wanted && page.core.connected && deviceRepeater.count === 0 && !page.searchTimedOut
        onTriggered: page.searchTimedOut = true
    }

    component FocusRing: Rectangle {
        // Kirigami's card background draws nothing for the keyboard focus; the card says when to show it (visualFocus: not after a click).
        property bool shown: false
        anchors.fill: parent
        z: 1
        radius: Kirigami.Units.cornerRadius
        color: "transparent"
        border.width: 2
        border.color: Kirigami.Theme.focusColor
        visible: shown
    }
    // Asks for a fresh look both ways: discovery over Wi-Fi Direct and one more Bluetooth scan.
    function searchAgain() {
        searchTimedOut = false;
        core.startDiscovery();
        core.scanBluetooth();
    }
    Component.onCompleted: if (wanted) core.startDiscovery()
    Component.onDestruction: core.stopDiscovery()

    function openFileDialog() {
        fileDialog.open();
    }

    // Pastes at most what still fits. A big text that went through the text box whole would first be laid
    // out in full, and cut only after that: seconds of freeze, and more with each repeated paste.
    // Files copied in a file manager, or an image: they are sent as files. True if there were any.
    function pasteFiles() {
        const files = core.clipboardFiles();
        if (files.length === 0) {
            return false;
        }
        // Each part of the path is escaped: "#", "?" and "%" are valid in file names but mean something in a URL.
        filesAdded(files.map(path => Qt.url("file://" + path.split("/").map(encodeURIComponent).join("/"))));
        return true;
    }

    // Everything the clipboard has: files first, otherwise text into the box (Ctrl+V, the tray).
    function pasteFromClipboard() {
        if (!pasteFiles()) {
            pasteText();
        }
    }

    function pasteText() {
        const start = textField.selectionStart;
        const end = textField.selectionEnd;
        const room = maxChars - textField.length + (end - start);
        if (room <= 0) {
            return;
        }
        const text = core.clipboardText(room);
        if (text.length > 0) {
            textField.remove(start, end);
            textField.insert(start, text);
        }
    }

    function sendTo(deviceId) {
        if (!hasContent) {
            return;
        }
        if (files.length > 0) {
            core.sendFiles(deviceId, files);
        } else {
            core.sendText(deviceId, trimmedText, singleLink && !sendAsText);
        }
        sent(deviceId);
    }

    // The most files listed in the card; the rest is in the dialog.
    readonly property int maxListed: 5

    Component {
        id: fileRow

        RowLayout {
            id: row

            required property var modelData

            spacing: Kirigami.Units.largeSpacing
            Layout.fillWidth: true

            Kirigami.Icon {
                source: row.modelData.icon
                Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                Layout.preferredHeight: Kirigami.Units.iconSizes.medium
            }
            ColumnLayout {
                spacing: 0
                Layout.fillWidth: true
                QQC2.Label {
                    text: row.modelData.name
                    elide: Text.ElideMiddle
                    Layout.fillWidth: true
                }
                QQC2.Label {
                    text: row.modelData.size
                    opacity: 0.7
                }
            }
            QQC2.ToolButton {
                icon.name: "edit-delete-remove"
                display: QQC2.AbstractButton.IconOnly
                // The file name tells the identical buttons of the rows apart for a screen reader.
                text: KI18n.i18nc("@action:button %1 is a file name", "Remove %1", row.modelData.name)
                QQC2.ToolTip.visible: hovered || visualFocus
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                QQC2.ToolTip.text: KI18n.i18nc("@info:tooltip", "Remove from the list of files to send")
                onClicked: page.fileRemoved(row.modelData.path)
            }
        }
    }

    Kirigami.Dialog {
        id: allFilesDialog
        title: KI18n.i18n("Files to Send")
        standardButtons: Kirigami.Dialog.NoButton
        footer: null // no button bar: the close button of the header is enough
        preferredWidth: Kirigami.Units.gridUnit * 22

        // A layout, not a ListView: rows get their width from the layout, so they all line up.
        QQC2.ScrollView {
            id: listScroll

            implicitHeight: Kirigami.Units.gridUnit * 22
            contentWidth: availableWidth
            leftPadding: Kirigami.Units.largeSpacing
            // The scroll bar sits in this space.
            rightPadding: QQC2.ScrollBar.vertical ? QQC2.ScrollBar.vertical.implicitWidth : 0
            Layout.fillWidth: true
            Layout.fillHeight: true

            ColumnLayout {
                width: listScroll.availableWidth
                spacing: Kirigami.Units.largeSpacing

                Repeater {
                    model: page.details
                    delegate: fileRow
                }
            }
        }

        Connections {
            target: page
            function onFilesChanged() {
                if (page.files.length <= page.maxListed) {
                    allFilesDialog.close();
                }
            }
        }
    }

    FileDialog {
        id: fileDialog
        title: KI18n.i18n("Choose Files to Send")
        fileMode: FileDialog.OpenFiles
        onAccepted: page.filesAdded(selectedFiles)
    }

    ColumnLayout {
        // Lets the placeholder below fill (and center in) the page when there is little content.
        height: Math.max(implicitHeight, page.availableHeight)
        spacing: Kirigami.Units.largeSpacing

        RowLayout {
            id: status

            readonly property bool starting: !page.core.connected && page.core.serviceStarting
            readonly property bool offline: !page.core.connected && !page.core.serviceStarting
            readonly property bool hidden: !page.core.visible
            readonly property bool dnd: page.core.suspended

            Layout.alignment: Qt.AlignHCenter
            spacing: Kirigami.Units.largeSpacing

            Kirigami.Icon {
                // Same icons as the matching options in Settings: hidden = visibility off, Do Not Disturb = notifications off.
                source: status.starting || status.offline ? "network-wireless-disconnected" : status.hidden ? "view-hidden" : status.dnd ? "notifications-disabled" : "network-wireless-hotspot"
                Layout.alignment: Qt.AlignVCenter
                Layout.preferredWidth: Kirigami.Units.iconSizes.large
                Layout.preferredHeight: Kirigami.Units.iconSizes.large
            }
            ColumnLayout {
                spacing: 0
                Layout.alignment: Qt.AlignVCenter

                Kirigami.Heading {
                    level: 3
                    text: status.starting ? KI18n.i18nc("@info:status", "Starting…") : status.offline ? KI18n.i18nc("@info:status", "Not running") : status.hidden ? KI18n.i18nc("@info:status", "Hidden") : status.dnd ? KI18n.i18n("Do Not Disturb") : KI18n.i18nc("@info:status", "Ready")
                }
                QQC2.Label {
                    visible: text.length > 0
                    opacity: 0.7
                    text: status.starting || status.offline ? "" : status.hidden ? KI18n.i18n("Turn on visibility in Preferences to receive files") : status.dnd ? KI18n.i18n("Hidden until Do Not Disturb ends") : KI18n.i18n("Visible as “%1”", page.core.deviceName)
                }
            }
        }

        Kirigami.AbstractCard {
            id: dropCard
            Layout.fillWidth: true
            // The height follows the content smoothly (drop zone, then the list of files).
            Layout.preferredHeight: implicitHeight
            Behavior on Layout.preferredHeight {
                NumberAnimation {
                    duration: Kirigami.Units.longDuration
                    easing.type: Easing.OutCubic
                }
            }
            highlighted: page.dragging
            // Also while files are queued, so that a drag shows the highlight.
            showClickFeedback: page.files.length === 0 || page.dragging
            focusPolicy: page.files.length === 0 ? Qt.StrongFocus : Qt.NoFocus
            Accessible.role: page.files.length === 0 ? Accessible.Button : Accessible.Grouping
            Accessible.name: page.files.length === 0 ? KI18n.i18nc("@action:button accessible name of the drop zone", "Add files to send") : KI18n.i18np("%1 file", "%1 files", page.files.length)
            onClicked: if (page.files.length === 0) fileDialog.open()

            FocusRing {
                shown: dropCard.visualFocus
            }

            contentItem: ColumnLayout {
                clip: true
                spacing: Kirigami.Units.largeSpacing

                ColumnLayout {
                    visible: page.files.length === 0
                    spacing: Kirigami.Units.smallSpacing
                    Layout.fillWidth: true
                    Layout.topMargin: Kirigami.Units.smallSpacing
                    Layout.bottomMargin: Kirigami.Units.smallSpacing

                    Kirigami.Icon {
                        source: page.dragging ? "document-import" : "folder-open"
                        Layout.alignment: Qt.AlignHCenter
                        Layout.preferredWidth: Kirigami.Units.iconSizes.large
                        Layout.preferredHeight: Kirigami.Units.iconSizes.large
                    }
                    Kirigami.Heading {
                        level: 3
                        text: page.dragging ? KI18n.i18n("Drop to share") : KI18n.i18n("Drop files here")
                        wrapMode: Text.Wrap
                        horizontalAlignment: Text.AlignHCenter
                        Layout.fillWidth: true
                    }
                    QQC2.Label {
                        text: KI18n.i18n("Or click to choose them")
                        opacity: 0.7
                        wrapMode: Text.Wrap
                        horizontalAlignment: Text.AlignHCenter
                        Layout.fillWidth: true
                    }
                    QQC2.Button {
                        icon.name: "edit-paste-symbolic"
                        text: KI18n.i18n("Paste from Clipboard")
                        enabled: page.core.clipboardHasFiles
                        Layout.topMargin: Kirigami.Units.smallSpacing * 2
                        Layout.alignment: Qt.AlignHCenter
                        onClicked: page.pasteFiles()
                    }
                }

                RowLayout {
                    visible: page.files.length > 0
                    Layout.fillWidth: true

                    Column {
                        Layout.fillWidth: true
                        Kirigami.Heading {
                            level: 3
                            text: KI18n.i18np("%1 file", "%1 files", page.files.length)
                        }
                        QQC2.Label {
                            text: page.core.formatSize(page.totalBytes)
                            opacity: 0.7
                        }
                    }
                    QQC2.Button {
                        icon.name: "list-add"
                        text: KI18n.i18nc("@action:button add files to the list to send", "Add")
                        onClicked: fileDialog.open()
                    }
                    QQC2.Button {
                        icon.name: "edit-clear-all-symbolic"
                        text: KI18n.i18nc("@action:button remove all files from the list to send", "Clear")
                        onClicked: page.filesCleared()
                    }
                }

                Kirigami.Separator {
                    visible: page.files.length > 0
                    Layout.fillWidth: true
                    Layout.topMargin: -Kirigami.Units.smallSpacing
                }

                Repeater {
                    model: page.files.length > 0 ? page.details.slice(0, page.maxListed) : []
                    delegate: fileRow
                }
                QQC2.Button {
                    visible: page.files.length > page.maxListed
                    flat: true
                    icon.name: "view-list-details"
                    text: KI18n.i18np("Show All %1 File", "Show All %1 Files", page.files.length)
                    Layout.alignment: Qt.AlignHCenter
                    onClicked: allFilesDialog.open()
                }
            }
        }

        Kirigami.AbstractCard {
            // Disabled, not hidden, while files are queued: files and text are never sent together.
            enabled: page.files.length === 0
            Layout.fillWidth: true

            contentItem: ColumnLayout {
                spacing: Kirigami.Units.largeSpacing

                RowLayout {
                    spacing: Kirigami.Units.largeSpacing

                    Kirigami.Icon {
                        source: "document-edit"
                        Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                        Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                    }
                    Kirigami.Heading {
                        level: 4
                        text: KI18n.i18n("Text or Link")
                        Layout.fillWidth: true
                    }
                    QQC2.Button {
                        display: QQC2.AbstractButton.IconOnly
                        enabled: textField.text.length > 0
                        icon.name: "edit-clear-symbolic"
                        text: KI18n.i18nc("@action:button empty the text box", "Clear")
                        QQC2.ToolTip.visible: hovered || visualFocus
                        QQC2.ToolTip.text: text
                        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                        onClicked: textField.clear()
                    }
                    // Text only: a copied image or file is pasted with the button of the card above
                    QQC2.Button {
                        display: QQC2.AbstractButton.IconOnly
                        icon.name: "edit-paste-symbolic"
                        text: KI18n.i18nc("@action:button paste text from the clipboard", "Paste")
                        enabled: page.core.clipboardHasText
                        QQC2.ToolTip.visible: hovered || visualFocus
                        QQC2.ToolTip.text: text
                        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                        onClicked: page.pasteText()
                    }
                }
                // One line at first, growing up to five lines, then it scrolls.
                QQC2.ScrollView {
                    id: textBox

                    readonly property real maxHeight: textField.topPadding + textField.bottomPadding + fontMetrics.lineSpacing * 5

                    Layout.fillWidth: true
                    Layout.preferredHeight: Math.min(textField.implicitHeight, maxHeight)

                    background: Rectangle {
                        radius: Kirigami.Units.cornerRadius
                        color: Qt.alpha(Kirigami.Theme.textColor, 0.05)
                        border.width: 1
                        border.color: textField.activeFocus ? Kirigami.Theme.highlightColor : Qt.alpha(Kirigami.Theme.textColor, 0.5)
                    }

                    FontMetrics {
                        id: fontMetrics
                        font: textField.font
                    }

                    QQC2.TextArea {
                        id: textField
                        // The desktop style paints a TextArea's placeholder with disabledTextColor (3.8:1) whatever placeholderTextColor says, so the page draws its own.
                        Accessible.name: KI18n.i18n("Type or paste something to send")
                        Component.onCompleted: text = page.draft
                        wrapMode: TextEdit.Wrap
                        leftPadding: Kirigami.Units.largeSpacing
                        rightPadding: Kirigami.Units.largeSpacing
                        topPadding: Kirigami.Units.largeSpacing
                        bottomPadding: Kirigami.Units.largeSpacing
                        background: null
                        QQC2.Label {
                            visible: textField.length === 0 && textField.preeditText.length === 0
                            x: textField.leftPadding
                            y: textField.topPadding
                            width: textField.width - textField.leftPadding - textField.rightPadding
                            text: textField.Accessible.name
                            color: Qt.alpha(Kirigami.Theme.textColor, 0.7)
                            wrapMode: Text.Wrap
                            Accessible.ignored: true
                        }
                        // Other ways of getting a longer text in (drag and drop, the context menu) are cut here.
                        onTextChanged: {
                            if (length > page.maxChars) {
                                remove(page.maxChars, length);
                            }
                            if (text !== page.draft) {
                                page.draftEdited(text);
                            }
                        }
                        Keys.onPressed: event => {
                            if (event.matches(StandardKey.Paste)) {
                                event.accepted = true;
                                // Holding the keys down pastes once, not dozens of times a second: the
                                // text would grow to the limit in an instant and each paste lays it out again.
                                if (!event.isAutoRepeat) {
                                    page.pasteFromClipboard();
                                }
                            }
                        }
                    }
                }

                RowLayout {
                    Layout.fillWidth: true

                    // Always shown. A single link can go either way (off = as a link). Without a valid
                    // link, or with more content around it, it can only go as text: on and locked, and the
                    // reason is behind a help button next to the checkbox, which is always there and only enabled while there is a reason, so the row does not change size.
                    QQC2.CheckBox {
                        id: asTextBox

                        readonly property string lockReason: textField.length === 0 || page.singleLink ? "" : page.startsWithLink ? KI18n.i18n("A link is sent as a link only when it is the only content") : KI18n.i18n("No valid link")

                        text: KI18n.i18n("Send as text")
                        checked: !page.singleLink || page.sendAsText
                        enabled: page.singleLink
                        Accessible.description: lockReason
                        onToggled: page.sendAsText = checked
                    }
                    Kirigami.ContextualHelpButton {
                        enabled: asTextBox.lockReason.length > 0
                        // Its tooltip also opens on hover, which a disabled button would show empty.
                        hoverEnabled: enabled
                        // A popup left open would stay with nothing to say.
                        onEnabledChanged: if (!enabled) toolTipVisible = false
                        toolTipText: asTextBox.lockReason
                    }
                    Item {
                        Layout.fillWidth: true
                    }
                    // How many characters out of the limit, only when it is getting close; the icon and the text at the limit carry the warning
                    Kirigami.Icon {
                        visible: page.nearLimit
                        source: "dialog-warning-symbolic"
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                    }
                    QQC2.Label {
                        visible: page.nearLimit
                        text: page.atLimit ? KI18n.i18nc("@info:status characters typed out of the most allowed, %1 and %2 are numbers; no more fit", "%1/%2 (limit reached)", Qt.locale().toString(textField.length, "f", 0), Qt.locale().toString(page.maxChars, "f", 0)) : KI18n.i18nc("@info:status characters typed out of the most allowed, %1 and %2 are numbers", "%1/%2", Qt.locale().toString(textField.length, "f", 0), Qt.locale().toString(page.maxChars, "f", 0))
                        opacity: 0.7
                        Layout.rightMargin: Kirigami.Units.smallSpacing
                    }
                }
            }
        }

        QQC2.Label {
            visible: page.files.length > 0
            text: KI18n.i18n("Remove the files to send text instead")
            opacity: 0.7
            wrapMode: Text.Wrap
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.smallSpacing
        }

        RowLayout {
            spacing: Kirigami.Units.largeSpacing
            Layout.topMargin: Kirigami.Units.smallSpacing

            Kirigami.Icon {
                source: "document-send"
                Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            }
            Kirigami.Heading {
                level: 3
                text: KI18n.i18n("Send to")
            }
            Item {
                Layout.fillWidth: true
            }
            // The one place to ask for a new search, whatever the state of the list.
            QQC2.ToolButton {
                enabled: page.core.connected
                icon.name: "view-refresh-symbolic"
                text: KI18n.i18n("Search Again")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered || visualFocus
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                QQC2.ToolTip.text: KI18n.i18n("Look for nearby devices again")
                onClicked: page.searchAgain()
            }
        }

        // Centered in the space left between the title and the bottom of the window
        Item {
            visible: deviceRepeater.count === 0
            Layout.fillWidth: true
            Layout.fillHeight: true
            Layout.minimumHeight: searching.implicitHeight + Kirigami.Units.gridUnit * 2

            Kirigami.PlaceholderMessage {
                id: searching

                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.gridUnit * 4
                // The service state comes from the core: a spinner only while it starts, none when it is down.
                readonly property string mode: !page.core.connected ? (page.core.serviceStarting ? "starting" : "offline") : page.searchTimedOut ? "empty" : "searching"

                icon.name: mode === "offline" ? "network-disconnect" : mode === "empty" ? "edit-find" : ""
                text: mode === "starting" ? KI18n.i18nc("@info:status", "Starting…") : mode === "offline" ? KI18n.i18n("Device search is not available") : mode === "empty" ? KI18n.i18n("No devices found") : KI18n.i18n("Looking for nearby devices…")
                explanation: mode === "empty" ? KI18n.i18n("Try searching again") : ""

                QQC2.ProgressBar {
                    visible: searching.mode === "starting" || searching.mode === "searching"
                    indeterminate: true
                    Layout.fillWidth: true
                }
                QQC2.Button {
                    visible: searching.mode === "empty"
                    flat: true
                    icon.name: "help-contents-symbolic"
                    text: KI18n.i18n("Open Help")
                    Layout.alignment: Qt.AlignHCenter
                    onClicked: page.showHelpRequested()
                }
            }
        }

        MessageBox {
            visible: page.core.transferBusy && deviceRepeater.count > 0
            type: Kirigami.MessageType.Information
            text: KI18n.i18n("Sending is paused while a transfer is in progress")
            actions: [
                Kirigami.Action {
                    text: KI18n.i18nc("@action:button", "Show Transfers")
                    icon.name: "folder-sync"
                    onTriggered: page.showTransfersRequested()
                }
            ]
            Layout.fillWidth: true
        }

        MessageBox {
            visible: deviceRepeater.count > 0 && !page.hasContent && !page.core.transferBusy
            text: KI18n.i18n("Choose or drop something to send, then pick a device")
            Layout.fillWidth: true
        }

        Repeater {
            id: deviceRepeater
            model: page.core.devices
            // A device showed up: the next empty list searches again instead of reporting "No devices found" at once.
            onCountChanged: if (count > 0) page.searchTimedOut = false

            delegate: Kirigami.AbstractCard {
                id: deviceCard

                required property string deviceId
                required property string name
                required property string deviceType
                required property string address
                required property string trust
                required property string medium

                readonly property string displayName: name.length > 0 ? name : medium === "ble" ? KI18n.i18n("Bluetooth device") : address
                readonly property string kindName: deviceType === "Tablet" ? KI18n.i18nc("@info kind of device", "Tablet") : deviceType === "Laptop" ? KI18n.i18nc("@info kind of device", "Laptop") : deviceType === "Phone" ? KI18n.i18nc("@info kind of device", "Phone") : KI18n.i18nc("@info kind of device", "Computer")

                Layout.fillWidth: true
                enabled: page.hasContent && !page.core.transferBusy
                showClickFeedback: true
                Accessible.role: Accessible.Button
                Accessible.name: KI18n.i18nc("@info accessible name of a device, %1 is its name and %2 its kind", "%1, %2", displayName, kindName)
                onClicked: page.sendTo(deviceId)

                FocusRing {
                    shown: deviceCard.visualFocus
                }

                // A newly found device fades in and rises a little.
                NumberAnimation on opacity {
                    from: 0
                    to: 1
                    duration: Kirigami.Units.longDuration
                }
                transform: Translate {
                    NumberAnimation on y {
                        from: Kirigami.Units.gridUnit
                        to: 0
                        duration: Kirigami.Units.longDuration
                        easing.type: Easing.OutCubic
                    }
                }

                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing

                    Kirigami.Icon {
                        source: deviceCard.deviceType === "Tablet" ? "tablet" : deviceCard.deviceType === "Laptop" ? "computer-laptop" : deviceCard.deviceType === "Phone" ? "smartphone" : "computer"
                        Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                        Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                    }
                    Kirigami.Heading {
                        level: 4
                        text: deviceCard.displayName
                        wrapMode: Text.Wrap
                        maximumLineCount: 2
                        elide: Text.ElideRight
                        Layout.fillWidth: true
                    }
                    RowLayout {
                        visible: deviceCard.medium === "ble"
                        spacing: Kirigami.Units.smallSpacing
                        Kirigami.Icon {
                            source: "network-bluetooth"
                            Layout.preferredWidth: Kirigami.Units.iconSizes.small
                            Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        }
                        QQC2.Label {
                            text: KI18n.i18n("Bluetooth")
                            opacity: 0.7
                        }
                    }
                    RowLayout {
                        visible: deviceCard.trust === "trusted"
                        spacing: Kirigami.Units.smallSpacing
                        Kirigami.Icon {
                            source: "security-high"
                            Layout.preferredWidth: Kirigami.Units.iconSizes.small
                            Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        }
                        QQC2.Label {
                            text: KI18n.i18n("Paired")
                            opacity: 0.7
                        }
                    }
                }
            }
        }

        // Takes the spare height when there are devices, so the other items stay together at the top
        Item {
            visible: deviceRepeater.count > 0
            Layout.fillHeight: true
        }
    }
}
