#include <QtTest>
#include <KLocalizedQmlContext>
#include <KLocalizedString>
#include <QQmlComponent>
#include <QQmlEngine>
#include <QQmlProperty>
#include <QQuickItem>
#include <QQuickWindow>

#include <algorithm>
#include <iterator>
#include <memory>
#include <utility>

// The shell's reusable QML components, driven the way a third-party app drives them: through the module URI, in an ApplicationWindow like Main.qml hosts them, and every test fails when the engine gives off a warning (collected in init, failed in cleanup).

namespace {

// What `core` is to `ToastStack`: the thing an error toast copies its text with. A stub, so the test cannot touch the real clipboard.
class FakeCore : public QObject
{
    Q_OBJECT

public:
    Q_INVOKABLE void copyToClipboard(const QString &text) { Q_UNUSED(text) }
};

// The stack hosted the way Main.qml hosts it: through the module URI, in an ApplicationWindow of the same document, where its declarative parent (an overlay) and its anchors meet a real window from the start.
constexpr auto toastsQml = R"QML(
import QtQuick.Controls
import io.github.the_beper.KuickShareTest
ApplicationWindow {
    id: win

    required property var core
    readonly property alias toasts: toasts

    ToastStack {
        id: toasts

        core: win.core
    }
}
)QML";

} // namespace

class QmlConsumerTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void init();
    void cleanup();

    void sectionTitleStartsFlushWithTheTop();

    void messageBoxShowsByDefault();

    void anErrorToastIsShownOnce();
    void aFullStackOfErrorsGivesWayOnlyToAnotherError();
    void aTimedToastLeavesOnItsOwnAndAnErrorWaits();
    void theStackOffersItsDocumentedDetailsSignal();

private:
    // A window out of `qml`, made the way Main.qml makes its own: the window is the root, so every declarative parent (an overlay, an anchor) and every binding finds what it looks for. The window stays shown until the test is over; nullptr is reported before it returns.
    std::unique_ptr<QQuickWindow> windowWith(const QByteArray &qml, const QVariantMap &properties);
    static bool invoke(QObject *object, const char *method, const QVariantList &arguments);
    // The number `name` ("Information", "Positive", "Warning", "Error") names in Kirigami.MessageType at this run, read from Kirigami itself.
    int messageType(const QString &name);

    FakeCore m_core;
    std::unique_ptr<QQmlEngine> m_engine;
    QList<QQmlError> m_warnings;
};

void QmlConsumerTest::init()
{
    m_engine = std::make_unique<QQmlEngine>();
    // The same setup main.cpp does: without it the KI18n singleton of the components is nothing, and the engine says so.
    KLocalization::setupLocalizedContext(m_engine.get());
    connect(m_engine.get(), &QQmlEngine::warnings, this, [this](const QList<QQmlError> &warnings) {
        m_warnings.append(warnings);
    });
}

void QmlConsumerTest::cleanup()
{
    m_engine.reset();
    QCoreApplication::sendPostedEvents(nullptr, QEvent::DeferredDelete);

    QStringList warnings;
    warnings.reserve(m_warnings.size());
    std::ranges::transform(m_warnings, std::back_inserter(warnings), &QQmlError::toString);
    m_warnings.clear();
    QVERIFY2(warnings.isEmpty(), qPrintable(warnings.join(u'\n')));
}

std::unique_ptr<QQuickWindow> QmlConsumerTest::windowWith(const QByteArray &qml,
                                                         const QVariantMap &properties)
{
    QQmlComponent component(m_engine.get());
    component.setData(qml, {});
    if (!component.isReady()) {
        qWarning().noquote() << component.errorString();
        return nullptr;
    }
    auto object = std::unique_ptr<QObject>{component.createWithInitialProperties(properties)};
    if (!object) {
        qWarning().noquote() << component.errorString();
        return nullptr;
    }
    auto *window = qobject_cast<QQuickWindow *>(object.release());
    if (!window) {
        qWarning() << "the root is not a window";
        return nullptr;
    }
    window->show();
    if (!QTest::qWaitForWindowExposed(window)) {
        qWarning() << "the window never got exposed";
        return nullptr;
    }
    return std::unique_ptr<QQuickWindow>{window};
}

bool QmlConsumerTest::invoke(QObject *object, const char *method, const QVariantList &arguments)
{
    return QMetaObject::invokeMethod(object, method, Q_ARG(QVariant, arguments.at(0)),
                                     Q_ARG(QVariant, arguments.at(1)),
                                     Q_ARG(QVariant, arguments.at(2)),
                                     Q_ARG(QVariant, arguments.at(3)));
}

int QmlConsumerTest::messageType(const QString &name)
{
    const QString probe =
        QStringLiteral("import QtQuick\nimport org.kde.kirigami as Kirigami\n"
                       "Item { readonly property int value: Kirigami.MessageType.%1 }")
            .arg(name);
    QQmlComponent component(m_engine.get());
    component.setData(probe.toUtf8(), {});
    if (!component.isReady()) {
        qWarning().noquote() << component.errorString();
        return -1;
    }
    auto object = std::unique_ptr<QObject>{component.create()};
    if (!object) {
        qWarning().noquote() << component.errorString();
        return -1;
    }
    return object->property("value").toInt();
}

//
// SectionTitle -----------------------------------------------------------------
//

void QmlConsumerTest::sectionTitleStartsFlushWithTheTop()
{
    // The margins read through the window's own properties, not through an attached property looked up from the other side.
    auto window = windowWith(R"QML(
import QtQuick.Controls
import QtQuick.Layouts
import io.github.the_beper.KuickShareTest
ApplicationWindow {
    id: win

    required property bool firstTitle
    readonly property alias heading: heading
    readonly property double topMargin: heading.Layout.topMargin
    readonly property double bottomMargin: heading.Layout.bottomMargin

    SectionTitle {
        id: heading

        text: "Transfers"
        first: win.firstTitle
    }
}
)QML",
                            {{QStringLiteral("firstTitle"), true}});
    QVERIFY(window);
    auto *heading =
        QQmlProperty::read(window.get(), QStringLiteral("heading")).value<QQuickItem *>();
    QVERIFY(heading);
    QCOMPARE(heading->property("level"), 4);
    // "The first title of a page needs no space above it: the page padding is enough."
    QCOMPARE(QQmlProperty::read(window.get(), QStringLiteral("topMargin")).toDouble(), 0.0);
    QVERIFY2(QQmlProperty::read(window.get(), QStringLiteral("bottomMargin")).toDouble() > 0,
             "every title keeps its space below");
    window->setProperty("firstTitle", false);
    QVERIFY2(QQmlProperty::read(window.get(), QStringLiteral("topMargin")).toDouble() > 0,
             "a title that follows the first keeps its space above");
    window->setProperty("firstTitle", true);
    QCOMPARE(QQmlProperty::read(window.get(), QStringLiteral("topMargin")).toDouble(), 0.0);
}

//
// MessageBox -------------------------------------------------------------------
//

void QmlConsumerTest::messageBoxShowsByDefault()
{
    auto window = windowWith(R"QML(
import QtQuick.Controls
import io.github.the_beper.KuickShareTest
ApplicationWindow {
    id: win

    readonly property alias box: box

    MessageBox {
        id: box

        text: "Needs Wi-Fi"
    }
}
)QML",
                             {});
    QVERIFY(window);
    auto *box = QQmlProperty::read(window.get(), QStringLiteral("box")).value<QQuickItem *>();
    QVERIFY(box);
    // Kirigami hides its InlineMessages by default; the shell's comment says the opposite.
    QCOMPARE(box->property("visible"), true);
    QCOMPARE(box->property("text"), QStringLiteral("Needs Wi-Fi"));
}

//
// ToastStack -------------------------------------------------------------------
//

void QmlConsumerTest::anErrorToastIsShownOnce()
{
    const int error = messageType(QStringLiteral("Error"));
    QVERIFY(error >= 0);
    auto window = windowWith(toastsQml, {{QStringLiteral("core"), QVariant::fromValue(&m_core)}});
    QVERIFY(window);
    auto *toasts = QQmlProperty::read(window.get(), QStringLiteral("toasts")).value<QObject *>();
    QVERIFY(toasts);

    // "an error already shown (same text, same transfer) is not added twice"
    QVERIFY(invoke(toasts, "show", {QVariant(error), QVariant(QStringLiteral("boom")),
                                    QVariant(0), QVariant(QStringLiteral("transfer-1"))}));
    const int first = toasts->property("lastId").toInt();
    QCOMPARE(first, 1);
    QVERIFY(invoke(toasts, "show", {QVariant(error), QVariant(QStringLiteral("boom")),
                                    QVariant(0), QVariant(QStringLiteral("transfer-1"))}));
    QCOMPARE(toasts->property("lastId").toInt(), first);
    // Another text or another transfer is a new toast.
    QVERIFY(invoke(toasts, "show", {QVariant(error), QVariant(QStringLiteral("boom")),
                                    QVariant(0), QVariant(QStringLiteral("transfer-2"))}));
    QCOMPARE(toasts->property("lastId").toInt(), 2);
    QVERIFY(invoke(toasts, "show", {QVariant(error), QVariant(QStringLiteral("boom2")),
                                    QVariant(0), QVariant(QStringLiteral("transfer-1"))}));
    QCOMPARE(toasts->property("lastId").toInt(), 3);
    // A warning is not a duplicate of an error, whatever the two texts say.
    const int warning = messageType(QStringLiteral("Warning"));
    QVERIFY(warning >= 0);
    QVERIFY(invoke(toasts, "show", {QVariant(warning), QVariant(QStringLiteral("boom")),
                                    QVariant(0), QVariant(QStringLiteral("transfer-1"))}));
    QCOMPARE(toasts->property("lastId").toInt(), 4);
}

void QmlConsumerTest::aFullStackOfErrorsGivesWayOnlyToAnotherError()
{
    const int information = messageType(QStringLiteral("Information"));
    QVERIFY(information >= 0);
    const int error = messageType(QStringLiteral("Error"));
    QVERIFY(error >= 0);
    auto window = windowWith(toastsQml, {{QStringLiteral("core"), QVariant::fromValue(&m_core)}});
    QVERIFY(window);
    auto *toasts = QQmlProperty::read(window.get(), QStringLiteral("toasts")).value<QObject *>();
    QVERIFY(toasts);

    const int maxVisible = toasts->property("maxVisible").toInt();
    QVERIFY2(maxVisible > 0, "the stack has a documented size");
    for (int n = 0; n < maxVisible; ++n) {
        QVERIFY(invoke(toasts, "show",
                       {QVariant(error), QVariant(QStringLiteral("error %1").arg(n + 1)),
                        QVariant(0), QVariant(QStringLiteral("transfer-%1").arg(n + 1))}));
    }
    const int before = toasts->property("lastId").toInt();
    QCOMPARE(before, maxVisible);

    // "any other message is dropped": a non-error cannot make room for itself.
    QVERIFY(invoke(toasts, "show", {QVariant(information), QVariant(QStringLiteral("done")),
                                    QVariant(1000), QVariant(QStringLiteral(""))}));
    QCOMPARE(toasts->property("lastId").toInt(), before);
    // "... and only another error can push one out of a full stack."
    QVERIFY(invoke(toasts, "show", {QVariant(error), QVariant(QStringLiteral("one more")),
                                    QVariant(0), QVariant(QStringLiteral("more"))}));
    QCOMPARE(toasts->property("lastId").toInt(), before + 1);
}

void QmlConsumerTest::aTimedToastLeavesOnItsOwnAndAnErrorWaits()
{
    const int positive = messageType(QStringLiteral("Positive"));
    QVERIFY(positive >= 0);
    const int error = messageType(QStringLiteral("Error"));
    QVERIFY(error >= 0);
    auto window = windowWith(toastsQml, {{QStringLiteral("core"), QVariant::fromValue(&m_core)}});
    QVERIFY(window);
    auto *toasts = QQmlProperty::read(window.get(), QStringLiteral("toasts")).value<QObject *>();
    QVERIFY(toasts);

    // "every message slides back down by itself after its time"
    QVERIFY(invoke(toasts, "show", {QVariant(positive), QVariant(QStringLiteral("quick")),
                                    QVariant(100), QVariant(QStringLiteral(""))}));
    QCOMPARE(toasts->property("lastId").toInt(), 1);
    QTRY_COMPARE(toasts->property("height").toDouble(), 0.0);

    // "Errors are the exception: they stay until closed, and can be copied"
    QVERIFY(invoke(toasts, "show", {QVariant(error), QVariant(QStringLiteral("waiting")),
                                    QVariant(100), QVariant(QStringLiteral(""))}));
    QCOMPARE(toasts->property("lastId").toInt(), 2);
    QTest::qWait(300);
    QVERIFY2(toasts->property("height").toDouble() > 0,
             "the time may be over, an error toast stays");
    QVERIFY(QMetaObject::invokeMethod(toasts, "dismiss", Q_ARG(QVariant, 2)));
    QTRY_COMPARE(toasts->property("height").toDouble(), 0.0);
}

void QmlConsumerTest::theStackOffersItsDocumentedDetailsSignal()
{
    auto window = windowWith(toastsQml, {{QStringLiteral("core"), QVariant::fromValue(&m_core)}});
    QVERIFY(window);
    auto *toasts = QQmlProperty::read(window.get(), QStringLiteral("toasts")).value<QObject *>();
    QVERIFY(toasts);
    QSignalSpy spy(toasts, SIGNAL(detailsRequested(QString)));
    QVERIFY(spy.isValid());
}

// The catalogs are not here, but the domain has to be: without it, every `KI18n.i18n` string warns the engine (and `cleanup` fails the test for it).
int main(int argc, char **argv)
{
    KLocalizedString::setApplicationDomain("kuickshare");
    QGuiApplication application(argc, argv);
    QmlConsumerTest test;

    return QTest::qExec(&test, argc, argv);
}

#include "qmlconsumer_test.moc"
