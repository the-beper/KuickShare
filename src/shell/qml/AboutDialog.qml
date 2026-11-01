import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.ki18n

// About KuickShare: the name and version, the license, the projects it builds on, the libraries in use.
// Everything sits directly on the dialog, without boxes.
Kirigami.Dialog {
    id: dialog

    required property var core

    // A title with a line under it; with an address the title is a link that opens in the browser. A link
    // is reachable by Tab, opens with Enter or Space, is read as a link, and is underlined and followed by
    // the external-link icon (`externalLink`, on by default since KF 6.11), so that the colour is not the only cue.
    component Entry: ColumnLayout {
        id: entry

        property string title
        property string description
        property string url

        spacing: 0
        Layout.fillWidth: true
        // Set, because a layout takes its maximum from its children otherwise: an entry with only the link,
        // whose maximum is the entry's width, would stay 0 wide.
        Layout.maximumWidth: Number.POSITIVE_INFINITY

        // Left-aligned like the titles and descriptions (LinkButton centres its text), and only as wide as
        // its text, so the empty room at its right does not open the link.
        Kirigami.UrlButton {
            url: entry.url
            text: entry.title
            visible: entry.url.length > 0
            font.underline: true
            wrapMode: Text.Wrap
            horizontalAlignment: Text.AlignLeft
            Accessible.role: Accessible.Link
            Layout.maximumWidth: entry.width
        }
        QQC2.Label {
            visible: entry.url.length === 0
            text: entry.title
            wrapMode: Text.Wrap
            Layout.fillWidth: true
        }
        QQC2.Label {
            visible: entry.description.length > 0
            text: entry.description
            opacity: 0.7
            wrapMode: Text.Wrap
            Layout.fillWidth: true
        }
    }

    // The room around the header icon and the close button, the same on every side, and the room around the content.
    readonly property real margin: Kirigami.Units.largeSpacing
    readonly property real contentMargin: Kirigami.Units.largeSpacing * 2
    readonly property real headerSize: Kirigami.Units.iconSizes.medium

    title: KI18n.i18n("About")
    standardButtons: Kirigami.Dialog.NoButton
    footer: null
    preferredWidth: Kirigami.Units.gridUnit * 24
    // A little room above and below for the opening animation, which moves the dialog: a dialog as tall as the
    // window would be clamped to it and stretch while it moves. A taller content scrolls.
    maximumHeight: absoluteMaximumHeight - Kirigami.Units.gridUnit

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
                source: "help-about-symbolic"
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

    // The dialog only sizes itself from an Item's implicitHeight, and it scrolls the content when the window is too
    // short. A ScrollView of our own that fills the dialog makes the height depend on the position, and the
    // opening animation (which moves the dialog) then stretches the dialog.
    // The content has its own margin on every side.
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
            // Tight, so the content fits the window without scrolling; the section titles bring their own space.
            spacing: Kirigami.Units.smallSpacing

            RowLayout {
                spacing: Kirigami.Units.largeSpacing * 2
                Layout.fillWidth: true

                Kirigami.Icon {
                    source: "io.github.the_beper.KuickShare"
                    fallback: "emblem-shared"
                    Layout.preferredWidth: Kirigami.Units.iconSizes.huge
                    Layout.preferredHeight: Kirigami.Units.iconSizes.huge
                }
                ColumnLayout {
                    spacing: 0
                    Layout.fillWidth: true

                    Kirigami.Heading {
                        level: 2
                        text: KI18n.i18nc("@label application name and version", "KuickShare %1", dialog.core.version)
                    }
                    QQC2.Label {
                        text: KI18n.i18n("Quick Share client for KDE Plasma")
                        opacity: 0.7
                        wrapMode: Text.Wrap
                        Layout.fillWidth: true
                    }
                    QQC2.Label {
                        text: "© 2026 the-beper"
                        opacity: 0.7
                    }
                }
            }

            Kirigami.Separator {
                Layout.fillWidth: true
                Layout.topMargin: Kirigami.Units.smallSpacing * 2
                Layout.bottomMargin: Kirigami.Units.smallSpacing * 2
            }
            Entry {
                title: KI18n.i18n("Homepage")
                url: "https://github.com/the-beper/KuickShare"
            }
            Entry {
                title: KI18n.i18n("Report a Bug")
                url: "https://github.com/the-beper/KuickShare/issues"
            }

            SectionTitle {
                text: KI18n.i18n("License")
                Layout.leftMargin: 0
            }
            Entry {
                title: KI18n.i18n("GPL-3.0-or-later")
                url: "https://www.gnu.org/licenses/gpl-3.0.html"
            }

            SectionTitle {
                text: KI18n.i18n("Built on")
                Layout.leftMargin: 0
            }
            Entry {
                title: KI18n.i18n("Packet")
                description: KI18n.i18n("The Quick Share client this started from, by nozwock")
                url: "https://github.com/nozwock/packet"
            }
            Entry {
                title: KI18n.i18n("rquickshare")
                description: KI18n.i18n("The Quick Share protocol library (rqs_lib), by nozwock and Martichou")
                url: "https://github.com/nozwock/rquickshare"
            }

            SectionTitle {
                text: KI18n.i18n("Libraries in Use")
                Layout.leftMargin: 0
            }
            Entry {
                title: KI18n.i18nc("@label library name and version", "Qt %1", dialog.core.qtVersion)
                description: KI18n.i18n("Cross-platform application framework")
            }
            Entry {
                title: KI18n.i18nc("@label library name and version", "KDE Frameworks %1", dialog.core.frameworksVersion)
                description: KI18n.i18n("Kirigami, notifications, jobs and the other KDE libraries")
            }
        }
    }
}
