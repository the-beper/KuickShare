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

    function capabilityTitle(key: string): string {
        switch (key) {
        case "sending":
            return KI18n.i18n("Sending");
        case "receiving":
            return KI18n.i18n("Receiving");
        }
        return KI18n.i18n("Join network");
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

    // One part of the system: its status icon and name, then "Ready" or, right under it, what needs attention and how
    // to fix it.
    component ComponentRow: RowLayout {
        id: componentItem

        required property var entry

        spacing: Kirigami.Units.largeSpacing

        Kirigami.Icon {
            source: componentItem.entry.iconName
            Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
            Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            Layout.alignment: Qt.AlignTop
            Accessible.ignored: true
        }
        ColumnLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing

            QQC2.Label {
                Layout.fillWidth: true
                text: componentItem.entry.title
                textFormat: Text.PlainText
                font.bold: true
                wrapMode: Text.Wrap
                // The icon only repeats the status: a screen reader gets it as text.
                Accessible.description: componentItem.entry.statusText
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: componentItem.entry.problems.length === 0
                text: componentItem.entry.statusText
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                opacity: 0.8
            }
            Repeater {
                model: componentItem.entry.problems
                delegate: ColumnLayout {
                    id: problem

                    required property var modelData

                    Layout.fillWidth: true
                    spacing: 0

                    QQC2.Label {
                        Layout.fillWidth: true
                        text: problem.modelData.text
                        textFormat: Text.PlainText
                        wrapMode: Text.Wrap
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        visible: text.length > 0
                        text: problem.modelData.hint
                        textFormat: Text.PlainText
                        wrapMode: Text.Wrap
                        opacity: 0.8
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        visible: problem.modelData.command.length > 0
                        spacing: Kirigami.Units.smallSpacing

                        // Selectable, so it can be copied even without the button.
                        QQC2.TextArea {
                            Layout.fillWidth: true
                            readOnly: true
                            text: problem.modelData.command
                            textFormat: TextEdit.PlainText
                            wrapMode: TextEdit.WrapAnywhere
                            font: Kirigami.Theme.fixedWidthFont
                            background: null
                            Accessible.name: componentItem.entry.title
                        }
                        QQC2.ToolButton {
                            Layout.alignment: Qt.AlignTop
                            icon.name: "edit-copy"
                            display: QQC2.AbstractButton.IconOnly
                            text: KI18n.i18n("Copy command")
                            onClicked: dialog.core.copyToClipboard(problem.modelData.command)
                        }
                    }
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

        ColumnLayout {
            Layout.fillWidth: true
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
                        source: capLine.modelData.cap.iconName
                        Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                        Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                        Accessible.ignored: true
                    }
                    QQC2.Label {
                        id: capTitle
                        text: dialog.capabilityTitle(capLine.modelData.key)
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
            // Why there is no current result, in plain words: no box, the list below stays as it was.
            QQC2.Label {
                Layout.fillWidth: true
                visible: text.length > 0
                text: {
                    if (dialog.wr.error === "unavailable" || !dialog.core.connected) {
                        return KI18n.i18n("KuickShare service unavailable");
                    }
                    if (dialog.wr.error === "schema") {
                        return KI18n.i18n("The result could not be read. Refresh to try again");
                    }
                    return dialog.hasReport ? "" : KI18n.i18n("Not checked yet");
                }
                wrapMode: Text.Wrap
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: dialog.hasReport && dialog.wr.generatedAtMs > 0
                text: {
                    const when = Qt.formatDateTime(new Date(dialog.wr.generatedAtMs), Qt.locale(), Locale.ShortFormat);
                    return dialog.wr.stale === true ? KI18n.i18n("Last checked: %1 (out of date)", when) : KI18n.i18n("Last checked: %1", when);
                }
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
                    model: dialog.hasReport ? dialog.wr.components : []
                    delegate: ComponentRow {
                        required property var modelData

                        Layout.fillWidth: true
                        entry: modelData
                    }
                }
            }
        }
    }
}
