import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.ki18n

Kirigami.ScrollablePage {
    id: page

    required property var core

    // The transfer model narrowed by the search text and the two filters of the list header.
    readonly property var filter: core.filteredTransfers

    title: KI18n.i18n("Transfers")

    // Search and filters, outside the list so they stay in place while it changes and scrolls. They stay visible
    // while nothing matches, so they can be changed back.
    header: QQC2.ToolBar {
        visible: page.filter.sourceCount > 0

        // Two columns: the search field above the two menus, and the two buttons, which are as wide
        // as each other. Every control is as high as the search field and "Clear History".
        contentItem: GridLayout {
            id: bar

            readonly property real controlHeight: Math.max(searchField.implicitHeight, clearHistoryButton.implicitHeight)
            readonly property real buttonWidth: Math.max(clearHistoryButton.implicitWidth, resetButton.implicitWidth)

            columns: 2
            columnSpacing: Kirigami.Units.smallSpacing
            rowSpacing: Kirigami.Units.smallSpacing

            Kirigami.SearchField {
                id: searchField

                Layout.fillWidth: true
                Layout.preferredHeight: bar.controlHeight
                placeholderText: KI18n.i18n("Search transfers")
                Accessible.name: KI18n.i18n("Search transfers")
                text: page.filter.text
                onTextChanged: page.filter.text = text
            }
            QQC2.Button {
                id: clearHistoryButton

                Layout.preferredWidth: bar.buttonWidth
                Layout.preferredHeight: bar.controlHeight
                text: KI18n.i18n("Clear History")
                icon.name: "edit-delete"
                onClicked: clearDialog.open()
            }

            RowLayout {
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true
                Layout.preferredHeight: bar.controlHeight

                QQC2.ComboBox {
                    id: directionBox

                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    Layout.preferredWidth: 1
                    textRole: "text"
                    valueRole: "value"
                    model: [
                        {text: KI18n.i18n("All transfers"), value: "all"},
                        {text: KI18n.i18nc("@item:inlistbox filter for transfers received", "Received"), value: "inbound"},
                        {text: KI18n.i18nc("@item:inlistbox filter for transfers sent", "Sent"), value: "outbound"}
                    ]
                    Accessible.name: KI18n.i18n("Filter by direction")
                    // The menu is as wide as the box.
                    popup.width: directionBox.width
                    onActivated: page.filter.direction = currentValue

                    // Follows the filter, which "Reset filters" also changes.
                    Binding {
                        target: directionBox
                        property: "currentIndex"
                        value: directionBox.count > 0 ? Math.max(0, directionBox.indexOfValue(page.filter.direction)) : -1
                    }
                }
                QQC2.ComboBox {
                    id: statusBox

                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    Layout.preferredWidth: 1
                    textRole: "text"
                    valueRole: "value"
                    model: [
                        {text: KI18n.i18n("Any status"), value: "all"},
                        {text: KI18n.i18n("In progress"), value: "active"},
                        {text: KI18n.i18n("Completed"), value: "done"},
                        {text: KI18n.i18n("Not completed"), value: "failed"}
                    ]
                    Accessible.name: KI18n.i18n("Filter by status")
                    popup.width: statusBox.width
                    onActivated: page.filter.status = currentValue

                    Binding {
                        target: statusBox
                        property: "currentIndex"
                        value: statusBox.count > 0 ? Math.max(0, statusBox.indexOfValue(page.filter.status)) : -1
                    }
                }
            }
            QQC2.Button {
                id: resetButton

                Layout.preferredWidth: bar.buttonWidth
                Layout.preferredHeight: bar.controlHeight
                text: KI18n.i18n("Reset Filters")
                icon.name: "edit-clear-all"
                enabled: page.filter.filtered
                onClicked: page.filter.clearFilters()
            }
        }
    }

    // Same padding on every page: cards float with rounded corners instead of touching the window edges.
    topPadding: Kirigami.Units.largeSpacing
    leftPadding: Kirigami.Units.largeSpacing
    rightPadding: Kirigami.Units.largeSpacing
    bottomPadding: Kirigami.Units.largeSpacing

    Kirigami.PromptDialog {
        id: clearDialog
        title: KI18n.i18n("Clear history?")
        subtitle: page.filter.filtered ? KI18n.i18n("The whole history is cleared, not only the transfers shown. Files you received are not touched") : KI18n.i18n("Files you received are not touched")
        // The accepting button is named after what it does, so the footer is made of actions.
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Clear History")
                icon.name: "edit-clear-history"
                onTriggered: {
                    page.core.clearHistory();
                    clearDialog.close();
                }
            },
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: clearDialog.close()
            }
        ]
    }

    required property var toasts

    // Asks before joining a received Wi-Fi network; filled by the "Join Network" button of a row.
    Kirigami.PromptDialog {
        id: joinDialog

        property string transferId: ""
        property string ssid: ""

        title: KI18n.i18n("Join this network?")
        subtitle: KI18n.i18n("Connect this system to “%1” and save it, so it connects again by itself. The current Wi-Fi connection is replaced", ssid)
        // The accepting button is named after what it does, so the footer is made of actions.
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Join Network")
                icon.name: "network-wireless"
                onTriggered: {
                    page.core.joinNetwork(joinDialog.transferId, joinDialog.ssid);
                    joinDialog.close();
                }
            },
            Kirigami.Action {
                text: KI18n.i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: joinDialog.close()
            }
        ]
    }

    // Size of the saved history above which a warning offers to clear it.
    readonly property real bigHistoryBytes: 5 * 1024 * 1024

    // What the details dialog shows; filled by showDetails().
    property string detailResult: ""
    property string detailSize: ""
    property var detailFiles: []
    property string detailText: ""
    property bool detailIsLink: false
    // The dialog is centred and at most 3/5 of the window high: a fifth stays free above and below.
    readonly property real detailsMaxHeight: Math.round((page.Window.height || 0) * 3 / 5)
    // What is left for the files or the text once the title, the header row and the paddings are taken
    // (an estimate in grid units, not measured).
    readonly property real detailsBodyMax: Math.max(Kirigami.Units.gridUnit * 6, detailsMaxHeight - Kirigami.Units.gridUnit * 9)

    function showDetails(item) {
        showInfo(item.transferId, {
            state: item.transferState,
            kind: item.kind,
            ack_bytes: item.ackBytes,
            total_bytes: item.totalBytes,
            files: item.files.join("\n"),
            saved_paths: item.savedPaths.join("\n"),
            text: item.shownText
        });
    }

    // Opens the dialog for a transfer by id (used by the "View" action of a notification).
    function showDetailsFor(id) {
        const info = core.transfers.info(id);
        if (info.state !== undefined) {
            showInfo(id, info);
        }
    }

    function showInfo(id, info) {
        const files = info.files ? info.files.split("\n").filter(f => f.length > 0) : [];
        const text = info.text && info.text.length > 0 ? info.text : (info.preview ?? "");
        detailResult = stateText(info.state, info.kind, info.ack_bytes, id.startsWith("ble:"));
        detailSize = info.total_bytes > 0 ? core.formatSize(info.total_bytes) : "";
        // Received files that are still on disk get an Open button; the saved name can differ from the offered one.
        const saved = info.kind === "inbound" && info.state === "Finished" && info.saved_paths ? info.saved_paths.split("\n").filter(f => f.length > 0) : [];
        detailFiles = (saved.length > 0 ? saved : files).map(path => {
            const name = path.substring(path.lastIndexOf("/") + 1);
            return {name: name, icon: core.iconForName(name), path: saved.length > 0 && core.fileExists(path) ? path : ""};
        });
        detailText = files.length === 0 ? text : "";
        detailIsLink = files.length === 0 && /^https?:\/\/\S+$/i.test(text.trim());
        detailsDialog.open();
    }

    Kirigami.Dialog {
        id: detailsDialog
        title: KI18n.i18n("Transfer Details")
        standardButtons: Kirigami.Dialog.NoButton
        footer: null
        preferredWidth: Math.min(page.Window.width - Kirigami.Units.gridUnit * 2, Kirigami.Units.gridUnit * 40)
        maximumHeight: page.detailsMaxHeight

        // The dialog only sizes itself from an Item's implicitHeight; a layout alone gives a collapsed dialog.
        Item {
            implicitHeight: detailsColumn.implicitHeight

            ColumnLayout {
                id: detailsColumn
                width: parent.width
                spacing: 0

                RowLayout {
                    spacing: Kirigami.Units.gridUnit * 2
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.rightMargin: Kirigami.Units.largeSpacing
                    Layout.bottomMargin: Kirigami.Units.largeSpacing

                    Repeater {
                        model: [[KI18n.i18n("Result"), page.detailResult], [KI18n.i18n("Size"), page.detailSize]]
                        delegate: ColumnLayout {
                            id: detail

                            required property var modelData

                            visible: modelData[1].length > 0
                            spacing: 0

                            QQC2.Label {
                                text: detail.modelData[0]
                                opacity: 0.7
                            }
                            Kirigami.Heading {
                                level: 4
                                text: detail.modelData[1]
                            }
                        }
                    }

                    Item {
                        Layout.fillWidth: true
                    }
                    // As tall as the two lines on the left.
                    QQC2.Button {
                        visible: page.detailIsLink
                        text: KI18n.i18n("Open Link")
                        icon.name: "internet-services"
                        Layout.fillHeight: true
                        onClicked: page.core.openLink(page.detailText.trim())
                    }
                    QQC2.Button {
                        visible: page.detailText.length > 0
                        text: KI18n.i18nc("@action:button copy the text to the clipboard", "Copy")
                        icon.name: "edit-copy"
                        Layout.fillHeight: true
                        onClicked: {
                            page.core.copyToClipboard(page.detailText);
                            page.toasts.show(Kirigami.MessageType.Positive, KI18n.i18n("Copied to clipboard"), 1500);
                        }
                    }
                }

                Kirigami.Separator {
                    visible: page.detailFiles.length > 0 || page.detailText.length > 0
                    Layout.fillWidth: true
                }

                QQC2.ScrollView {
                    id: filesScroll

                    visible: page.detailFiles.length > 0
                    implicitHeight: Math.min(filesColumn.implicitHeight, page.detailsBodyMax)
                    contentWidth: availableWidth
                    leftPadding: Kirigami.Units.largeSpacing
                    rightPadding: QQC2.ScrollBar.vertical ? QQC2.ScrollBar.vertical.implicitWidth : 0
                    Layout.fillWidth: true

                    ColumnLayout {
                        id: filesColumn

                        // One row: the text line plus the space around it.
                        readonly property real rowHeight: Math.max(fileMetrics.height, Kirigami.Units.iconSizes.smallMedium, openProbe.implicitHeight) + Kirigami.Units.largeSpacing * 2 + spacing

                        width: filesScroll.availableWidth
                        spacing: 0

                        FontMetrics {
                            id: fileMetrics
                        }

                        // Only measures the Open button, which the taller rows contain.
                        QQC2.Button {
                            id: openProbe
                            visible: false
                            text: KI18n.i18nc("@action:button open a received file", "Open")
                            icon.name: "document-open"
                        }

                        Repeater {
                            model: page.detailFiles
                            delegate: RowLayout {
                                id: fileItem

                                required property var modelData

                                spacing: Kirigami.Units.largeSpacing
                                Layout.fillWidth: true
                                // Even space around the row: this is what rowHeight adds up.
                                Layout.topMargin: Kirigami.Units.largeSpacing
                                Layout.bottomMargin: Kirigami.Units.largeSpacing

                                Kirigami.Icon {
                                    source: fileItem.modelData.icon
                                    Layout.alignment: Qt.AlignVCenter
                                    Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                                    Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                                }
                                QQC2.Label {
                                    text: fileItem.modelData.name
                                    textFormat: Text.PlainText
                                    elide: Text.ElideMiddle
                                    verticalAlignment: Text.AlignVCenter
                                    Layout.alignment: Qt.AlignVCenter
                                    Layout.fillWidth: true
                                }
                                QQC2.Button {
                                    visible: fileItem.modelData.path.length > 0
                                    text: KI18n.i18nc("@action:button open a received file", "Open")
                                    icon.name: "document-open"
                                    onClicked: page.core.openFile(fileItem.modelData.path)
                                }
                            }
                        }
                    }
                }

                QQC2.ScrollView {
                    id: textScroll

                    visible: page.detailFiles.length === 0 && page.detailText.length > 0
                    implicitHeight: Math.min(textLabel.implicitHeight, page.detailsBodyMax)
                    contentWidth: availableWidth
                    leftPadding: Kirigami.Units.largeSpacing
                    rightPadding: QQC2.ScrollBar.vertical ? QQC2.ScrollBar.vertical.implicitWidth : 0
                    Layout.fillWidth: true

                    // Read-only, but the text can be selected and copied.
                    QQC2.TextArea {
                        id: textLabel

                        width: textScroll.availableWidth
                        text: page.detailText
                        readOnly: true
                        selectByMouse: true
                        wrapMode: Text.Wrap
                        textFormat: TextEdit.PlainText
                        background: null
                        topPadding: Kirigami.Units.largeSpacing
                        bottomPadding: Kirigami.Units.largeSpacing
                        leftPadding: 0
                        color: page.detailIsLink ? Kirigami.Theme.linkColor : Kirigami.Theme.textColor
                    }
                }
            }
        }
    }

    function stateText(state, kind, ackBytes, viaBluetooth) {
        switch (state) {
        case "Starting":
            // The send was requested but the core has not reported anything yet.
            return viaBluetooth ? KI18n.i18n("Looking for the device…") : KI18n.i18n("Connecting…");
        case "WaitingForUserConsent":
            return KI18n.i18n("Waiting for your approval");
        case "ReceivingFiles":
            return KI18n.i18n("Receiving…");
        case "SentIntroduction":
            return kind === "outbound" ? KI18n.i18n("Waiting for the other device to accept") : KI18n.i18n("Connecting…");
        case "SendingFiles":
            // Over Bluetooth nothing moves until the Wi-Fi link the other device offers is up.
            return kind === "outbound" && viaBluetooth && ackBytes === 0 ? KI18n.i18n("Switching to Wi-Fi…") : KI18n.i18n("Sending…");
        case "Finished":
            return kind === "outbound" ? KI18n.i18nc("@info:status transfer result", "Sent") : KI18n.i18nc("@info:status transfer result", "Received");
        case "Cancelled":
            return KI18n.i18nc("@info:status transfer result", "Cancelled");
        case "Rejected":
            return kind === "outbound" ? KI18n.i18n("Declined") : KI18n.i18n("You declined");
        case "Disconnected":
            // The library reports a refusal by the other device as a disconnection.
            return kind === "outbound" && ackBytes === 0 ? KI18n.i18n("Not accepted") : KI18n.i18n("Interrupted");
        default:
            return KI18n.i18n("Connecting…");
        }
    }

    function describe(files, text, preview) {
        if (files.length > 0) {
            return KI18n.i18np("%1 file", "%1 files", files.length);
        }
        return text.length > 0 ? text : preview;
    }

    // Two parts of one line; a translation can change the separator or the order.
    function joined(first, second) {
        return KI18n.i18nc("@info:status two parts of a status line, %1 and %2 are texts", "%1 · %2", first, second);
    }

    // "2 minutes left"; rounded up so the last seconds never read "0 minutes".
    function etaText(seconds) {
        if (seconds < 60) {
            return KI18n.i18np("%1 second left", "%1 seconds left", Math.max(1, seconds));
        }
        if (seconds < 3600) {
            return KI18n.i18np("%1 minute left", "%1 minutes left", Math.ceil(seconds / 60));
        }
        return KI18n.i18np("%1 hour left", "%1 hours left", Math.ceil(seconds / 3600));
    }

    function when(seconds) {
        return seconds > 0 ? Qt.locale().toString(new Date(seconds * 1000), Locale.ShortFormat) : "";
    }

    ListView {
        id: list
        model: page.filter
        spacing: Kirigami.Units.largeSpacing
        bottomMargin: Kirigami.Units.largeSpacing

        // No ListView transitions: a card that changes height while one runs leaves its neighbours overlapping,
        // or half transparent. Entries animate themselves instead (see the delegate).

        header: ColumnLayout {
            x: Kirigami.Units.largeSpacing
            width: list.width - Kirigami.Units.largeSpacing * 2
            height: implicitHeight + Kirigami.Units.largeSpacing
            spacing: Kirigami.Units.largeSpacing

            // The saved history grows without limit: past this size, offer to clear it.
            MessageBox {
                visible: page.core.historyBytes > page.bigHistoryBytes
                type: Kirigami.MessageType.Warning
                text: KI18n.i18n("The history is large (%1) and may slow KuickShare down", page.core.formatSize(page.core.historyBytes))
                actions: [
                    Kirigami.Action {
                        text: KI18n.i18n("Clear History")
                        icon.name: "edit-delete"
                        onTriggered: clearDialog.open()
                    }
                ]
                Layout.fillWidth: true
                Layout.topMargin: Kirigami.Units.largeSpacing
            }

            MessageBox {
                visible: !page.core.historyEnabled
                type: Kirigami.MessageType.Information
                text: KI18n.i18n("History is turned off: new transfers are not saved. You can turn it on in Preferences")
                Layout.fillWidth: true
            }
        }

        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 4
            visible: list.count === 0
            icon.name: page.filter.sourceCount > 0 ? "edit-find" : page.core.connected ? "folder-sync" : "dialog-warning"
            text: page.filter.sourceCount > 0 ? KI18n.i18n("No matching transfers") : page.core.connected ? KI18n.i18n("No transfers yet") : KI18n.i18n("Starting the sharing service…")
            explanation: page.filter.sourceCount > 0 || !page.core.connected ? "" : page.core.historyEnabled ? KI18n.i18n("Sent and received transfers are listed here. Make sure this system is visible in Preferences to receive.") : KI18n.i18n("Transfers are only listed while the app is open, because the history is turned off.")
        }

        delegate: Item {
            id: cell

            required property string transferId
            required property string kind
            required property string transferState
            required property string device
            required property string pin
            required property string preview
            required property real totalBytes
            required property real ackBytes
            required property real speed
            required property real etaSeconds
            required property var savedPaths
            required property var files
            required property string bodyText
            required property string payloadKind
            required property real time
            required property string trusted
            required property string alwaysAccepted
            required property real consent
            required property string reason
            required property int index

            // A vertical ListView positions its items itself and ignores side margins, so the card
            // is inset inside this full-width cell.
            width: ListView.view.width
            implicitHeight: item.implicitHeight
            height: item.height

            // How far the card is drawn from its cell while it slides into place; the cell itself is always
            // where the list put it. A new entry starts a little above, and when the cell moves (an entry was
            // added or removed above it, or a card above changed height) the card starts where it was.
            property real shift: 0
            property real lastY: NaN

            onYChanged: {
                if (!isNaN(lastY) && lastY !== y) {
                    shift += lastY - y;
                    slide.restart();
                }
                lastY = y;
            }
            ListView.onAdd: {
                shift = -Kirigami.Units.gridUnit;
                slide.restart();
                fadeIn.restart();
            }
            // The entry stays in the list while it fades out.
            ListView.onRemove: removeAnimation.start()

            SequentialAnimation {
                id: removeAnimation

                PropertyAction {
                    target: cell
                    property: "ListView.delayRemove"
                    value: true
                }
                NumberAnimation {
                    target: item
                    property: "opacity"
                    to: 0
                    duration: Kirigami.Units.shortDuration
                }
                PropertyAction {
                    target: cell
                    property: "ListView.delayRemove"
                    value: false
                }
            }

            NumberAnimation {
                id: fadeIn

                target: item
                property: "opacity"
                from: 0
                to: 1
                duration: Kirigami.Units.longDuration
            }

            NumberAnimation {
                id: slide

                target: cell
                property: "shift"
                to: 0
                duration: Kirigami.Units.longDuration
                easing.type: Easing.OutCubic
            }

            Kirigami.AbstractCard {
                id: item

                x: Kirigami.Units.largeSpacing
                y: cell.shift
                width: parent.width - Kirigami.Units.largeSpacing * 2

                readonly property string transferId: cell.transferId
                readonly property bool viaBluetooth: cell.transferId.startsWith("ble:")
                readonly property string kind: cell.kind
                readonly property string transferState: cell.transferState
                readonly property string device: cell.device
                readonly property string pin: cell.pin
                readonly property string preview: cell.preview
                readonly property real totalBytes: cell.totalBytes
                readonly property real ackBytes: cell.ackBytes
                readonly property real speed: cell.speed
                readonly property real etaSeconds: cell.etaSeconds
                readonly property var savedPaths: cell.savedPaths
                readonly property var files: cell.files
                readonly property string bodyText: cell.bodyText
                readonly property string payloadKind: cell.payloadKind
                readonly property real time: cell.time
                readonly property string trusted: cell.trusted
                readonly property string alwaysAccepted: cell.alwaysAccepted
                // The core's token of the consent that waits for an answer; without it the buttons would answer nothing.
                readonly property real consent: cell.consent
                readonly property string reason: cell.reason
                readonly property int index: cell.index

                readonly property bool awaitingConsent: kind === "inbound" && transferState === "WaitingForUserConsent"
                readonly property bool active: transferState !== "Finished" && transferState !== "Cancelled" && transferState !== "Rejected" && transferState !== "Disconnected"
                readonly property bool inProgress: active && !awaitingConsent && totalBytes > 0
                // "Receiving · 12 MB · 4.2 MB/s · 2 minutes left": speed and time appear once there is an estimate.
                readonly property string progressText: {
                    let text = page.joined(page.stateText(transferState, kind, ackBytes, viaBluetooth).replace(/…|\.\.\./, ""), page.core.formatSize(totalBytes));
                    if (speed > 0) {
                        text = page.joined(text, KI18n.i18nc("@info:status transfer speed, %1 is a size", "%1/s", page.core.formatSize(speed)));
                        if (etaSeconds >= 0) {
                            text = page.joined(text, page.etaText(etaSeconds));
                        }
                    }
                    return text;
                }
                // An outbound send can be cancelled while it looks for the device or connects.
                readonly property bool cancellable: inProgress || (kind === "outbound" && active)
                readonly property bool receivedFiles: kind === "inbound" && transferState === "Finished" && files.length > 0
                readonly property bool hasText: transferState === "Finished" && files.length === 0 && (bodyText.length > 0 || preview.length > 0)
                readonly property string content: page.describe(files, bodyText, preview)
                readonly property string shownText: bodyText.length > 0 ? bodyText : preview
                // Wi-Fi details just received (the core keeps the password for a few minutes, not in the history).
                readonly property bool joinable: kind === "inbound" && transferState === "Finished" && payloadKind === "WiFi" && shownText.length > 0 && !transferId.startsWith("history-")
                readonly property bool isLink: /^https?:\/\/\S+$/i.test(shownText.trim())

                padding: Kirigami.Units.largeSpacing
                highlighted: awaitingConsent

                contentItem: ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing

                    RowLayout {
                        spacing: Kirigami.Units.largeSpacing
                        Layout.fillWidth: true

                        Kirigami.Icon {
                            source: item.files.length === 0 && item.content.length > 0 ? (item.isLink ? "internet-services" : "format-justify-left") : (item.kind === "outbound" ? "document-send" : "folder-download")
                            Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                            Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                            Layout.alignment: Qt.AlignTop
                        }

                        ColumnLayout {
                            spacing: Kirigami.Units.smallSpacing
                            Layout.fillWidth: true

                            RowLayout {
                                spacing: Kirigami.Units.largeSpacing
                                Layout.fillWidth: true

                                RowLayout {
                                    spacing: Kirigami.Units.largeSpacing
                                    Layout.fillWidth: true
                                    // Keeps a long name (or the shield) away from the buttons on the right.
                                    Layout.rightMargin: Kirigami.Units.largeSpacing * 2

                                    Kirigami.Heading {
                                        id: deviceName

                                        level: 4
                                        text: item.device.length > 0 ? item.device : KI18n.i18n("Unknown device")
                                        elide: Text.ElideRight
                                        // As wide as the name, but it gives way (and is elided) when the row is short.
                                        Layout.fillWidth: true
                                        Layout.minimumWidth: 0
                                        // Whole pixels: a layout rounds a width like 84.14 down to 84, and the text would be elided.
                                        Layout.maximumWidth: Math.ceil(implicitWidth)

                                        HoverHandler {
                                            id: nameHover
                                        }
                                        QQC2.ToolTip.visible: nameHover.hovered && deviceName.truncated
                                        QQC2.ToolTip.text: deviceName.text
                                        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                                    }
                                    // Shown as text too: a tooltip needs hover.
                                    RowLayout {
                                        visible: item.kind === "inbound" && (item.alwaysAccepted.length > 0 || item.trusted.length > 0)
                                        spacing: Kirigami.Units.smallSpacing
                                        Kirigami.Icon {
                                            source: "security-high"
                                            // The label next to it names it.
                                            Accessible.ignored: true
                                            Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                                            Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                                        }
                                        QQC2.Label {
                                            text: KI18n.i18nc("@info:status the sender is trusted", "Trusted")
                                            opacity: 0.7
                                            elide: Text.ElideRight
                                        }
                                    }
                                    Item {
                                        Layout.fillWidth: true
                                    }
                                }
                                QQC2.Label {
                                    visible: item.inProgress
                                    text: item.totalBytes > 0 ? KI18n.i18nc("@info:progress percent of a transfer, %1 is a number", "%1%", Math.round(100 * item.ackBytes / item.totalBytes)) : ""
                                    font.weight: Font.DemiBold
                                }
                                QQC2.ToolButton {
                                    visible: item.cancellable
                                    icon.name: "process-stop"
                                    display: QQC2.AbstractButton.IconOnly
                                    // The device name tells the identical buttons of the rows apart for a screen reader.
                                    text: KI18n.i18nc("@action:button %1 is a device name", "Cancel Transfer with %1", deviceName.text)
                                    QQC2.ToolTip.visible: hovered
                                    QQC2.ToolTip.text: KI18n.i18nc("@info:tooltip", "Cancel transfer")
                                    onClicked: page.core.cancel(item.transferId)
                                }
                                QQC2.ToolButton {
                                    visible: !item.active
                                    icon.name: "edit-delete-remove"
                                    display: QQC2.AbstractButton.IconOnly
                                    text: KI18n.i18nc("@action:button %1 is a device name", "Remove Transfer with %1 from List", deviceName.text)
                                    QQC2.ToolTip.visible: hovered
                                    QQC2.ToolTip.text: KI18n.i18n("Remove from list")
                                    onClicked: page.core.removeTransfer(page.filter.sourceRow(item.index))
                                }
                            }

                            RowLayout {
                                spacing: Kirigami.Units.largeSpacing
                                Layout.fillWidth: true

                                QQC2.Label {
                                    readonly property string status: item.reason.length > 0 && !item.active ? page.core.refusalText(item.reason) : item.awaitingConsent && item.pin.length > 0 ? KI18n.i18n("Wants to share with you · PIN %1", item.pin) : item.inProgress ? item.progressText : page.stateText(item.transferState, item.kind, item.ackBytes, item.viaBluetooth)
                                    readonly property string stamp: page.when(item.time)
                                    text: stamp.length > 0 && !item.active ? page.joined(status, stamp) : status
                                    opacity: 0.7
                                    elide: Text.ElideRight
                                    Layout.fillWidth: true
                                }
                                QQC2.Button {
                                    visible: !item.active
                                    text: KI18n.i18n("More Details")
                                    icon.name: "help-about-symbolic"
                                    onClicked: page.showDetails(item)
                                }
                            }

                            RowLayout {
                                visible: item.content.length > 0 || item.awaitingConsent
                                spacing: Kirigami.Units.largeSpacing
                                Layout.fillWidth: true

                                QQC2.Label {
                                    // The size is already in the status line while a transfer runs.
                                    // One line: the first one, with "…" when the text goes on (otherwise it is only elided if it does not fit).
                                    readonly property var lines: item.content.split("\n").map(l => l.trim()).filter(l => l.length > 0)
                                    readonly property string first: (lines.length > 0 ? lines[0] : "") + (lines.length > 1 ? "…" : "")
                                    text: item.totalBytes > 0 && item.files.length > 0 && !item.inProgress ? page.joined(first, page.core.formatSize(item.totalBytes)) : first
                                    textFormat: Text.PlainText
                                    wrapMode: Text.NoWrap
                                    elide: item.files.length > 0 ? Text.ElideMiddle : Text.ElideRight
                                    // A link is clickable and reachable by keyboard: it opens in the browser. It is underlined
                                    // (not only coloured) and shows a focus frame.
                                    color: linkTarget ? Kirigami.Theme.linkColor : Kirigami.Theme.textColor
                                    font.underline: linkTarget
                                    activeFocusOnTab: linkTarget
                                    Layout.fillWidth: true

                                    readonly property bool linkTarget: item.isLink && item.files.length === 0

                                    function openLink() {
                                        page.core.openLink(item.shownText.trim());
                                    }

                                    Accessible.role: linkTarget ? Accessible.Link : Accessible.StaticText
                                    Accessible.name: text
                                    Accessible.onPressAction: {
                                        if (linkTarget) {
                                            openLink();
                                        }
                                    }
                                    Keys.onPressed: event => {
                                        if (linkTarget && (event.key === Qt.Key_Return || event.key === Qt.Key_Enter || event.key === Qt.Key_Space)) {
                                            event.accepted = true;
                                            openLink();
                                        }
                                    }

                                    Rectangle {
                                        anchors.fill: parent
                                        anchors.margins: -Kirigami.Units.smallSpacing
                                        visible: parent.activeFocus
                                        color: "transparent"
                                        border.width: 1
                                        border.color: Kirigami.Theme.highlightColor
                                        radius: Kirigami.Units.cornerRadius
                                    }

                                    HoverHandler {
                                        enabled: parent.linkTarget
                                        cursorShape: Qt.PointingHandCursor
                                    }
                                    TapHandler {
                                        enabled: parent.linkTarget
                                        onTapped: parent.openLink()
                                    }
                                }
                                QQC2.Button {
                                    visible: item.awaitingConsent && item.consent > 0
                                    text: KI18n.i18nc("@action:button accept an incoming transfer", "Accept")
                                    icon.name: "dialog-ok-apply"
                                    onClicked: page.core.accept(item.transferId)
                                }
                                QQC2.Button {
                                    visible: item.awaitingConsent && item.consent > 0
                                    text: KI18n.i18nc("@action:button decline an incoming transfer", "Decline")
                                    icon.name: "dialog-cancel"
                                    onClicked: page.core.decline(item.transferId)
                                }
                                QQC2.Button {
                                    visible: item.receivedFiles
                                    text: KI18n.i18n("Open Folder")
                                    icon.name: "document-open-folder"
                                    onClicked: page.core.openDownloadFolder()
                                }
                                QQC2.Button {
                                    visible: item.joinable
                                    text: KI18n.i18n("Join Network")
                                    icon.name: "network-wireless"
                                    onClicked: {
                                        joinDialog.transferId = item.transferId;
                                        joinDialog.ssid = item.shownText;
                                        joinDialog.open();
                                    }
                                }
                                QQC2.Button {
                                    visible: item.hasText
                                    text: KI18n.i18nc("@action:button copy the text to the clipboard", "Copy")
                                    icon.name: "edit-copy"
                                    onClicked: {
                                        page.core.copyToClipboard(item.shownText);
                                        page.toasts.show(Kirigami.MessageType.Positive, KI18n.i18n("Copied to clipboard"), 1500);
                                    }
                                }
                            }

                            QQC2.ProgressBar {
                                visible: item.inProgress
                                from: 0
                                to: item.totalBytes
                                value: item.ackBytes
                                // Updates come about twice a second: glide between them.
                                Behavior on value {
                                    NumberAnimation {
                                        duration: Kirigami.Units.veryLongDuration
                                    }
                                }
                                Layout.fillWidth: true
                            }
                        }
                    }
                }
            }
        }
    }
}
