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
    onWantedChanged: wanted ? core.startDiscovery() : core.stopDiscovery()
    Component.onCompleted: if (wanted) core.startDiscovery()
    Component.onDestruction: core.stopDiscovery()

    function openFileDialog() {
        fileDialog.open();
    }
    function clearText() {
        textField.clear();
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
                    font: Kirigami.Theme.smallFont
                }
            }
            QQC2.ToolButton {
                icon.name: "edit-delete-remove"
                display: QQC2.AbstractButton.IconOnly
                // The file name tells the identical buttons of the rows apart for a screen reader.
                text: KI18n.i18nc("@action:button %1 is a file name", "Remove %1", row.modelData.name)
                QQC2.ToolTip.visible: hovered
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

            readonly property bool starting: !page.core.connected
            readonly property bool hidden: !page.core.visible
            readonly property bool dnd: page.core.suspended

            Layout.alignment: Qt.AlignHCenter
            spacing: Kirigami.Units.largeSpacing

            Kirigami.Icon {
                // Same icons as the matching options in Settings: hidden = visibility off, Do Not Disturb = notifications off.
                source: status.starting ? "network-wireless-disconnected" : status.hidden ? "view-hidden" : status.dnd ? "notifications-disabled" : "network-wireless-hotspot"
                Layout.alignment: Qt.AlignVCenter
                Layout.preferredWidth: Kirigami.Units.iconSizes.large
                Layout.preferredHeight: Kirigami.Units.iconSizes.large
            }
            ColumnLayout {
                spacing: 0
                Layout.alignment: Qt.AlignVCenter

                Kirigami.Heading {
                    level: 3
                    color: status.starting || status.hidden || status.dnd ? Kirigami.Theme.textColor : Kirigami.Theme.highlightColor
                    text: status.starting ? KI18n.i18nc("@info:status", "Starting…") : status.hidden ? KI18n.i18nc("@info:status", "Hidden") : status.dnd ? KI18n.i18n("Do Not Disturb") : KI18n.i18nc("@info:status", "Ready")
                }
                QQC2.Label {
                    visible: text.length > 0
                    opacity: 0.7
                    text: status.starting ? "" : status.hidden ? KI18n.i18n("Turn on visibility in Preferences to receive files") : status.dnd ? KI18n.i18n("Hidden until Do Not Disturb ends") : KI18n.i18n("Visible as “%1”", page.core.deviceName)
                }
            }
        }

        Kirigami.AbstractCard {
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
            showClickFeedback: page.files.length === 0
            onClicked: if (page.files.length === 0) fileDialog.open()

            contentItem: ColumnLayout {
                clip: true
                spacing: Kirigami.Units.largeSpacing

                Kirigami.PlaceholderMessage {
                    visible: page.files.length === 0
                    Layout.fillWidth: true
                    Layout.topMargin: Kirigami.Units.largeSpacing
                    Layout.bottomMargin: Kirigami.Units.largeSpacing
                    icon.name: page.dragging ? "document-import" : "folder-open"
                    icon.width: Kirigami.Units.iconSizes.huge
                    icon.height: Kirigami.Units.iconSizes.huge
                    text: page.dragging ? KI18n.i18n("Drop to share") : KI18n.i18n("Drop files here")
                    explanation: KI18n.i18n("Or click to choose them")
                    helpfulAction: Kirigami.Action {
                        icon.name: "edit-paste"
                        text: KI18n.i18n("Paste from Clipboard")
                        enabled: page.core.clipboardHasFiles
                        onTriggered: page.pasteFiles()
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
                        icon.name: "edit-clear-all"
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
            visible: page.files.length === 0
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
                    // Always laid out (just hidden), so the card does not change height while typing.
                    QQC2.Button {
                        opacity: textField.text.length > 0 ? 1 : 0
                        Behavior on opacity {
                            NumberAnimation {
                                duration: Kirigami.Units.shortDuration
                            }
                        }
                        enabled: textField.text.length > 0
                        icon.name: "edit-clear-all"
                        text: KI18n.i18nc("@action:button empty the text box", "Clear")
                        onClicked: textField.clear()
                    }
                    // Text only: a copied image or file is pasted with the button of the card above
                    QQC2.Button {
                        icon.name: "edit-paste"
                        text: KI18n.i18nc("@action:button paste text from the clipboard", "Paste")
                        enabled: page.core.clipboardHasText
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
                        border.color: textField.activeFocus ? Kirigami.Theme.highlightColor : Qt.alpha(Kirigami.Theme.textColor, 0.2)
                    }

                    FontMetrics {
                        id: fontMetrics
                        font: textField.font
                    }

                    QQC2.TextArea {
                        id: textField
                        placeholderText: KI18n.i18n("Type or paste something to send")
                        wrapMode: TextEdit.Wrap
                        leftPadding: Kirigami.Units.largeSpacing
                        rightPadding: Kirigami.Units.largeSpacing
                        topPadding: Kirigami.Units.largeSpacing
                        bottomPadding: Kirigami.Units.largeSpacing
                        background: null
                        // Other ways of getting a longer text in (drag and drop, the context menu) are cut here.
                        onTextChanged: if (length > page.maxChars) remove(page.maxChars, length)
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
                    // reason is written on the same row, against the right edge (not only in a tooltip).
                    QQC2.CheckBox {
                        id: asTextBox

                        text: KI18n.i18n("Send as text")
                        checked: !page.singleLink || page.sendAsText
                        enabled: page.singleLink
                        Accessible.description: lockReason.text
                        onToggled: page.sendAsText = checked
                    }
                    QQC2.Label {
                        id: lockReason

                        text: page.singleLink ? "" : page.startsWithLink ? KI18n.i18n("A string can only be sent as a link when it is the only content") : KI18n.i18n("No valid link")
                        opacity: 0.7
                        wrapMode: Text.Wrap
                        horizontalAlignment: Text.AlignRight
                        Layout.fillWidth: true
                        Layout.rightMargin: Kirigami.Units.smallSpacing
                    }
                    // How many characters out of the limit, in yellow, only when it is getting close
                    QQC2.Label {
                        visible: page.nearLimit
                        text: KI18n.i18nc("@info:status characters typed out of the most allowed, %1 and %2 are numbers", "%1/%2", Qt.locale().toString(textField.length, "f", 0), Qt.locale().toString(page.maxChars, "f", 0))
                        color: Kirigami.Theme.neutralTextColor
                        font: Kirigami.Theme.smallFont
                        Layout.rightMargin: Kirigami.Units.smallSpacing
                    }
                }
            }
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
            // Devices in Receive mode are found over Bluetooth by a short scan, not continuously.
            QQC2.ToolButton {
                visible: page.core.wifiUpgrade
                icon.name: "view-refresh-symbolic"
                text: KI18n.i18n("Search Again")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: KI18n.i18n("Search for nearby devices over Bluetooth again")
                onClicked: page.core.scanBluetooth()
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
                text: KI18n.i18n("Looking for nearby devices…")

                QQC2.ProgressBar {
                    indeterminate: true
                    Layout.fillWidth: true
                }
            }
        }

        MessageBox {
            visible: page.core.transferBusy && deviceRepeater.count > 0
            type: Kirigami.MessageType.Information
            text: KI18n.i18n("A transfer is in progress. You can send again when it ends")
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

            delegate: Kirigami.AbstractCard {
                id: deviceCard

                required property string deviceId
                required property string name
                required property string deviceType
                required property string address
                required property string trust
                required property string medium

                Layout.fillWidth: true
                enabled: page.hasContent && !page.core.transferBusy
                showClickFeedback: true
                onClicked: page.sendTo(deviceId)

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
                        text: deviceCard.name.length > 0 ? deviceCard.name : deviceCard.medium === "ble" ? KI18n.i18n("Bluetooth device") : deviceCard.address
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
