pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.ki18n

// What Wi-Fi Direct needs on this computer, as the core reports it. The dialog only renders the
// report held by the bridge: it never checks anything itself, and checking changes nothing on the system.
Kirigami.Dialog {
    id: dialog

    required property var core

    readonly property var wr: core.wifiRequirements
    readonly property bool hasReport: wr.valid === true
    readonly property bool busy: core.wifiRequirementsBusy

    function sectionTitle(key: string, fallback: string): string {
        switch (key) {
        case "sending":
            return KI18n.i18n("Sending");
        case "receiving":
            return KI18n.i18n("Receiving");
        case "join_network":
            return KI18n.i18n("Join network");
        case "system":
            return KI18n.i18n("System components");
        }
        return fallback;
    }

    // The status is always written out; this icon only repeats it.
    function capabilityIcon(status: string): string {
        switch (status) {
        case "available":
            return "emblem-ok-symbolic";
        case "degraded":
        case "limited":
            return "dialog-warning-symbolic";
        case "unavailable":
            return "dialog-error-symbolic";
        }
        return "dialog-question-symbolic";
    }

    // Kirigami builds the footer buttons from the actions: look for Refresh to give it the focus.
    function findButton(item: Item, label: string): Item {
        if (!item) {
            return null;
        }
        if (item instanceof QQC2.AbstractButton && item.text === label) {
            return item;
        }
        for (const child of item.children) {
            const found = findButton(child, label);
            if (found) {
                return found;
            }
        }
        return null;
    }

    title: KI18n.i18n("System requirements")
    preferredWidth: Kirigami.Units.gridUnit * 34
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton

    onOpened: {
        // The last report is shown at once; the core runs a new check only when it is out of date.
        if (!hasReport || wr.stale === true) {
            core.loadWifiRequirements();
        }
        Qt.callLater(() => {
            const button = findButton(dialog.footer, refreshAction.text);
            if (button) {
                button.forceActiveFocus();
            }
        });
    }

    customFooterActions: [
        Kirigami.Action {
            id: refreshAction
            text: KI18n.i18n("Refresh")
            icon.name: "view-refresh"
            enabled: !dialog.busy && dialog.core.connected
            onTriggered: dialog.core.refreshWifiRequirements()
        },
        Kirigami.Action {
            text: KI18n.i18n("Copy diagnostic report")
            icon.name: "edit-copy"
            enabled: dialog.hasReport
            onTriggered: dialog.core.copyWifiRequirementsReport()
        },
        Kirigami.Action {
            text: KI18n.i18n("Close")
            icon.name: "window-close"
            onTriggered: dialog.close()
        }
    ]

    // One component of the system: status as text and icon, a short reason, details on demand.
    component RequirementRow: ColumnLayout {
        id: rowItem

        required property var row

        spacing: Kirigami.Units.smallSpacing

        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.largeSpacing

            Kirigami.Icon {
                source: rowItem.row.iconName
                Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                Layout.alignment: Qt.AlignTop
                Accessible.ignored: true
            }
            ColumnLayout {
                Layout.fillWidth: true
                spacing: 0

                QQC2.Label {
                    Layout.fillWidth: true
                    text: rowItem.row.title
                    textFormat: Text.PlainText
                    font.bold: true
                    wrapMode: Text.Wrap
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    text: rowItem.row.statusText
                    textFormat: Text.PlainText
                    wrapMode: Text.Wrap
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    visible: text.length > 0
                    text: rowItem.row.reasonText
                    textFormat: Text.PlainText
                    wrapMode: Text.Wrap
                    opacity: 0.8
                }
            }
            QQC2.Button {
                id: detailsButton
                Layout.alignment: Qt.AlignTop
                text: KI18n.i18n("Details")
                checkable: true
                Accessible.name: rowItem.row.title + ", " + text
                Accessible.description: rowItem.row.statusText
            }
        }

        ColumnLayout {
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.iconSizes.smallMedium + Kirigami.Units.largeSpacing
            visible: detailsButton.checked
            spacing: Kirigami.Units.smallSpacing

            QQC2.Label {
                Layout.fillWidth: true
                visible: text.length > 0
                text: rowItem.row.hint
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: text.length > 0
                text: rowItem.row.details
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: rowItem.row.affectsText.length > 0
                text: KI18n.i18n("Affects: %1", rowItem.row.affectsText)
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: rowItem.row.verification.length > 0
                text: KI18n.i18n("How it was checked: %1", rowItem.row.verification)
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
            }
            RowLayout {
                Layout.fillWidth: true
                visible: rowItem.row.command.length > 0
                spacing: Kirigami.Units.smallSpacing

                // Selectable, so it can be copied even without the button.
                QQC2.TextArea {
                    Layout.fillWidth: true
                    readOnly: true
                    text: rowItem.row.command
                    textFormat: TextEdit.PlainText
                    wrapMode: TextEdit.WrapAnywhere
                    font: Kirigami.Theme.fixedWidthFont
                    background: null
                    Accessible.name: rowItem.row.title
                }
                QQC2.Button {
                    Layout.alignment: Qt.AlignTop
                    text: KI18n.i18n("Copy command")
                    icon.name: "edit-copy"
                    onClicked: dialog.core.copyToClipboard(rowItem.row.command)
                }
            }
        }
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        // Header: what this is, the overall status per direction, when it was checked.
        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.largeSpacing

            QQC2.Label {
                Layout.fillWidth: true
                text: KI18n.i18n("What Wi-Fi Direct needs on this computer. Checking never changes Bluetooth, Wi-Fi or the firewall")
                wrapMode: Text.Wrap
            }
            QQC2.BusyIndicator {
                running: dialog.busy
                visible: running
                Accessible.name: KI18n.i18n("Refresh")
            }
        }

        MessageBox {
            visible: dialog.wr.error === "unavailable" || !dialog.core.connected
            type: Kirigami.MessageType.Error
            text: KI18n.i18n("KuickShare service unavailable")
            Layout.fillWidth: true
        }
        MessageBox {
            visible: dialog.wr.error === "schema"
            type: Kirigami.MessageType.Warning
            text: KI18n.i18n("The result could not be read. Refresh to try again")
            Layout.fillWidth: true
        }
        MessageBox {
            visible: dialog.hasReport && dialog.wr.stale === true
            type: Kirigami.MessageType.Warning
            text: KI18n.i18n("These results are out of date. Refresh to check again")
            Layout.fillWidth: true
        }

        QQC2.Label {
            Layout.fillWidth: true
            visible: !dialog.hasReport && dialog.wr.error !== "unavailable" && dialog.wr.error !== "schema"
            text: KI18n.i18n("Not checked yet")
            wrapMode: Text.Wrap
        }

        ColumnLayout {
            Layout.fillWidth: true
            visible: dialog.hasReport
            spacing: Kirigami.Units.smallSpacing

            Repeater {
                model: dialog.hasReport ? [
                    {
                        key: "sending",
                        cap: dialog.wr.capabilities.sending
                    },
                    {
                        key: "receiving",
                        cap: dialog.wr.capabilities.receiving
                    },
                    {
                        key: "join_network",
                        cap: dialog.wr.capabilities.joinNetwork
                    }
                ] : []
                delegate: RowLayout {
                    id: capLine

                    required property var modelData

                    Layout.fillWidth: true
                    spacing: Kirigami.Units.largeSpacing
                    Accessible.role: Accessible.StaticText
                    Accessible.name: capTitle.text + ": " + capLabel.text

                    Kirigami.Icon {
                        source: dialog.capabilityIcon(capLine.modelData.cap.status)
                        Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                        Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                        Accessible.ignored: true
                    }
                    QQC2.Label {
                        id: capTitle
                        text: dialog.sectionTitle(capLine.modelData.key, "")
                        font.bold: true
                        Accessible.ignored: true
                    }
                    QQC2.Label {
                        id: capLabel
                        Layout.fillWidth: true
                        text: capLine.modelData.cap.label
                        textFormat: Text.PlainText
                        wrapMode: Text.Wrap
                        Accessible.ignored: true
                    }
                }
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: dialog.hasReport && dialog.wr.generatedAtMs > 0
                text: KI18n.i18n("Last checked: %1", Qt.formatDateTime(new Date(dialog.wr.generatedAtMs), Qt.locale(), Locale.ShortFormat))
                wrapMode: Text.Wrap
                opacity: 0.8
            }
        }

        Kirigami.Separator {
            Layout.fillWidth: true
            visible: dialog.hasReport
        }

        QQC2.ScrollView {
            id: scroll

            Layout.fillWidth: true
            Layout.fillHeight: true
            Layout.preferredHeight: Kirigami.Units.gridUnit * 18
            visible: dialog.hasReport
            Accessible.name: dialog.title
            QQC2.ScrollBar.horizontal.policy: QQC2.ScrollBar.AlwaysOff

            ColumnLayout {
                width: scroll.availableWidth
                spacing: Kirigami.Units.largeSpacing

                Repeater {
                    model: dialog.hasReport ? dialog.wr.sections : []
                    delegate: ColumnLayout {
                        id: sectionItem

                        required property var modelData

                        Layout.fillWidth: true
                        spacing: Kirigami.Units.smallSpacing

                        Kirigami.Heading {
                            Layout.fillWidth: true
                            level: 3
                            text: dialog.sectionTitle(sectionItem.modelData.key, sectionItem.modelData.title)
                            textFormat: Text.PlainText
                            wrapMode: Text.Wrap
                            Accessible.role: Accessible.Heading
                        }
                        QQC2.Label {
                            Layout.fillWidth: true
                            visible: sectionItem.modelData.rows.length === 0
                            text: KI18n.i18n("Nothing limits this")
                            wrapMode: Text.Wrap
                        }
                        Repeater {
                            model: sectionItem.modelData.rows
                            delegate: RequirementRow {
                                required property var modelData

                                Layout.fillWidth: true
                                row: modelData
                            }
                        }
                    }
                }
            }
        }
    }
}
