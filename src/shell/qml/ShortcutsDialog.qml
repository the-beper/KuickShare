pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.ki18n

// The keyboard shortcuts, in an overlay: each key in a small box, the action next to it.
Kirigami.Dialog {
    id: dialog

    readonly property var rows: [
        ["Ctrl+1", KI18n.i18nc("@item:intable shortcut action", "Send")],
        ["Ctrl+2", KI18n.i18nc("@item:intable shortcut action", "Transfers")],
        ["Ctrl+F", KI18n.i18n("Search Transfers")],
        ["Ctrl+,", KI18n.i18nc("@item:intable shortcut action", "Preferences")],
        ["Ctrl+H", KI18n.i18nc("@item:intable shortcut action", "Help")],
        ["F1", KI18n.i18nc("@item:intable shortcut action", "Help")],
        ["Ctrl+?", KI18n.i18n("Show Shortcuts")],
        ["Ctrl+O", KI18n.i18n("Choose Files to Send")],
        ["Ctrl+V", KI18n.i18n("Paste from Clipboard")],
        ["Ctrl+L", KI18n.i18n("Clear Files and Text")],
        ["Ctrl+W", KI18n.i18n("Close Window")],
        ["Ctrl+Q", KI18n.i18n("Quit KuickShare")]
    ]

    function keyParts(keys) {
        const parts = [];
        keys.split("+").forEach((key, i) => {
            if (i > 0) {
                parts.push("+");
            }
            parts.push(key);
        });
        return parts;
    }

    // The room around the header icon and the close button, the same on every side, and the room around the content.
    readonly property real margin: Kirigami.Units.largeSpacing
    readonly property real contentMargin: Kirigami.Units.largeSpacing * 2
    readonly property real headerSize: Kirigami.Units.iconSizes.medium

    title: KI18n.i18n("Shortcuts")
    standardButtons: Kirigami.Dialog.NoButton
    footer: null
    preferredWidth: Kirigami.Units.gridUnit * 24
    // Room above and below for the opening animation, which moves the dialog: a dialog as tall as the window
    // would be clamped to it and stretch while it moves. A taller content scrolls.
    maximumHeight: absoluteMaximumHeight - Kirigami.Units.gridUnit * 3

    header: QQC2.Control {
        // Every side set on its own: the desktop style ignores `padding` for the top and bottom.
        // The header spans the dialog border (1 px at the top and the sides); the content sits inside it.
        topPadding: dialog.margin + 1
        bottomPadding: dialog.margin
        leftPadding: dialog.margin + 1
        rightPadding: dialog.margin + 1
        // The icon and the close button are the same size, so the header has the same room above, below and at the
        // sides of what you see: the icon, and the box of the button (it shows when focused).
        contentItem: RowLayout {
            spacing: Kirigami.Units.largeSpacing

            Kirigami.Icon {
                source: "input-keyboard-symbolic"
                Layout.preferredWidth: dialog.headerSize
                Layout.preferredHeight: dialog.headerSize
            }
            Kirigami.Heading {
                level: 3
                text: dialog.title
                Layout.fillWidth: true
            }
            QQC2.ToolButton {
                icon.name: "window-close"
                icon.width: Kirigami.Units.iconSizes.smallMedium
                icon.height: Kirigami.Units.iconSizes.smallMedium
                display: QQC2.AbstractButton.IconOnly
                text: KI18n.i18nc("@action:button", "Close")
                Layout.preferredWidth: dialog.headerSize
                Layout.preferredHeight: dialog.headerSize
                onClicked: dialog.close()
            }
        }
    }

    // The dialog only sizes itself from an Item's implicitHeight; a layout alone gives a collapsed dialog.
    Item {
        implicitHeight: column.implicitHeight + dialog.contentMargin * 2

        ColumnLayout {
            id: column

            anchors {
                left: parent.left
                right: parent.right
                top: parent.top
                leftMargin: dialog.contentMargin
                rightMargin: dialog.contentMargin
                topMargin: dialog.contentMargin
            }
            spacing: Kirigami.Units.largeSpacing

            Repeater {
                model: dialog.rows
                delegate: RowLayout {
                    id: row

                    required property var modelData

                    spacing: Kirigami.Units.largeSpacing * 2
                    Layout.fillWidth: true

                    RowLayout {
                        spacing: Kirigami.Units.smallSpacing
                        // One width for all the rows, so the keys end on the same line and the
                        // descriptions start on the same line.
                        Layout.minimumWidth: Kirigami.Units.gridUnit * 8
                        Layout.maximumWidth: Kirigami.Units.gridUnit * 8
                        Layout.preferredWidth: Kirigami.Units.gridUnit * 8
                        Layout.alignment: Qt.AlignVCenter

                        Item {
                            Layout.fillWidth: true
                        }
                        Repeater {
                            model: dialog.keyParts(row.modelData[0])
                            delegate: Rectangle {
                                id: key

                                required property string modelData
                                readonly property bool plus: modelData === "+"

                                radius: Kirigami.Units.cornerRadius
                                color: plus ? "transparent" : Qt.alpha(Kirigami.Theme.textColor, 0.06)
                                border.width: plus ? 0 : 1
                                border.color: Qt.alpha(Kirigami.Theme.textColor, 0.25)
                                implicitWidth: plus ? label.implicitWidth : Math.max(label.implicitWidth + Kirigami.Units.largeSpacing * 2, implicitHeight)
                                implicitHeight: label.implicitHeight + Kirigami.Units.smallSpacing * 2

                                QQC2.Label {
                                    id: label

                                    anchors.centerIn: parent
                                    text: key.modelData
                                    opacity: key.plus ? 0.7 : 1
                                }
                            }
                        }
                    }
                    QQC2.Label {
                        text: row.modelData[1]
                        wrapMode: Text.Wrap
                        Layout.fillWidth: true
                        Layout.minimumWidth: 0
                    }
                }
            }
        }
    }
}
