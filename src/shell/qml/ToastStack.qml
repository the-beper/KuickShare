pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.ki18n

// Short coloured messages over the window: each one slides up from the bottom edge, older ones are pushed
// up, and every message slides back down by itself after its time (or when clicked).
// Errors are the exception: they stay until closed, and can be copied; an error already shown (same text, same
// transfer) is not added twice, and only another error can push one out of a full stack.
Item {
    id: stack

    required property var core

    readonly property int maxVisible: 4
    property int lastId: 0

    // The "Show Details" action of a toast was used; `id` is the one given to `show`.
    signal detailsRequested(string id)

    // `type` is a Kirigami.MessageType; the timeout (ms) is ignored for errors. A non-empty `detailId`
    // (a transfer id) adds an action that asks for the details of that transfer.
    function show(type, text, timeout, detailId = "") {
        if (type === Kirigami.MessageType.Error) {
            for (let i = 0; i < model.count; ++i) {
                const shown = model.get(i);
                if (shown.kind === Kirigami.MessageType.Error && shown.body === text && shown.detailId === detailId) {
                    return;
                }
            }
        }
        if (model.count >= maxVisible) {
            // The oldest message that goes away by itself; when all are errors (unread until closed), the oldest
            // error makes room for a new error only, and any other message is dropped.
            let victim = model.count - 1;
            for (let i = model.count - 1; i >= 0; --i) {
                if (model.get(i).kind !== Kirigami.MessageType.Error) {
                    victim = i;
                    break;
                }
            }
            if (model.get(victim).kind === Kirigami.MessageType.Error && type !== Kirigami.MessageType.Error) {
                return;
            }
            model.remove(victim);
        }
        model.insert(0, {
            uid: ++lastId,
            kind: type,
            body: text,
            timeout: timeout,
            detailId: detailId
        });
    }

    function dismiss(uid) {
        for (let i = 0; i < model.count; ++i) {
            if (model.get(i).uid === uid) {
                model.remove(i);
                return;
            }
        }
    }

    parent: QQC2.Overlay.overlay
    z: 100
    width: Math.round(parent.width * 0.7)
    height: list.contentHeight
    anchors.bottom: parent.bottom
    anchors.bottomMargin: Kirigami.Units.gridUnit * 2
    anchors.horizontalCenter: parent.horizontalCenter

    ListView {
        id: list

        anchors.fill: parent
        interactive: false
        spacing: Kirigami.Units.smallSpacing
        // The newest message (index 0) sits at the bottom.
        verticalLayoutDirection: ListView.BottomToTop

        model: ListModel {
            id: model
        }

        add: Transition {
            ParallelAnimation {
                NumberAnimation {
                    property: "y"
                    from: ViewTransition.destination.y + Kirigami.Units.gridUnit * 4
                    duration: Kirigami.Units.longDuration
                    easing.type: Easing.OutCubic
                }
                NumberAnimation {
                    property: "opacity"
                    from: 0
                    to: 1
                    duration: Kirigami.Units.longDuration
                }
            }
        }
        displaced: Transition {
            NumberAnimation {
                property: "y"
                duration: Kirigami.Units.longDuration
                easing.type: Easing.OutCubic
            }
        }
        // The mirror of `add`: slides down and fades out.
        remove: Transition {
            ParallelAnimation {
                NumberAnimation {
                    property: "y"
                    to: ViewTransition.item.y + Kirigami.Units.gridUnit * 4
                    duration: Kirigami.Units.longDuration
                    easing.type: Easing.InCubic
                }
                NumberAnimation {
                    property: "opacity"
                    to: 0
                    duration: Kirigami.Units.longDuration
                }
            }
        }

        delegate: MessageBox {
            id: toast

            required property int uid
            required property int kind
            required property string body
            required property int timeout
            required property string detailId

            readonly property bool persistent: kind === Kirigami.MessageType.Error

            width: ListView.view.width
            type: kind
            text: body
            showCloseButton: persistent
            actions: [
                Kirigami.Action {
                    visible: toast.detailId.length > 0
                    text: KI18n.i18n("Show Details")
                    icon.name: "view-list-details"
                    onTriggered: {
                        stack.detailsRequested(toast.detailId);
                        stack.dismiss(toast.uid);
                    }
                },
                // The text may carry something to copy (a command), so every error without other action has it.
                Kirigami.Action {
                    visible: toast.persistent && toast.detailId.length === 0
                    text: KI18n.i18nc("@action:button", "Copy")
                    icon.name: "edit-copy"
                    onTriggered: stack.core.copyToClipboard(toast.body)
                }
            ]

            // The close button hides the message; take it out of the list too. A window that is hidden
            // hides its items as well (the list included): then the message must stay.
            onVisibleChanged: {
                // A delegate being released may have no view any more.
                if (!visible && ListView.view && ListView.view.visible) {
                    stack.dismiss(toast.uid);
                }
            }

            // A click closes the message before its time, unless there is something to click in it.
            TapHandler {
                enabled: !toast.persistent
                onTapped: stack.dismiss(toast.uid)
            }

            Timer {
                running: !toast.persistent && toast.timeout > 0
                interval: toast.timeout
                onTriggered: stack.dismiss(toast.uid)
            }
        }
    }
}
