#include "instance.h"

#include <QDBusConnection>
#include <QDBusConnectionInterface>
#include <QDBusMessage>
#include <QDBusReply>
#include <QElapsedTimer>
#include <QThread>

namespace
{
    const QString shellService = QStringLiteral("org.kde.kuickshare");
    const QString coreService = QStringLiteral("io.github.the_beper.KuickShare");

    bool registered(const QString &name)
    {
        const QDBusReply<bool> reply = QDBusConnection::sessionBus().interface()->isServiceRegistered(name);
        return reply.isValid() && reply.value();
    }

    bool waitGone(const QString &name, int timeoutMs)
    {
        QElapsedTimer timer;
        timer.start();
        while (registered(name))
        {
            if (timer.elapsed() > timeoutMs)
            {
                return false;
            }
            QThread::msleep(100);
        }
        return true;
    }
}

bool Instance::shouldReplace(const QString &running, const QString &own, bool force)
{
    return !running.isEmpty() && (force || running != own);
}

void Instance::replaceRunning(bool force)
{
    auto bus = QDBusConnection::sessionBus();
    if (!bus.isConnected() || !registered(shellService))
    {
        return;
    }
    auto get = QDBusMessage::createMethodCall(shellService, QStringLiteral("/MainApplication"), QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("Get"));
    get << QStringLiteral("org.qtproject.Qt.QCoreApplication") << QStringLiteral("applicationVersion");
    const QDBusReply<QVariant> version = bus.call(get, QDBus::Block, 2000);
    if (!version.isValid() && !force)
    {
        // A busy shell of this very version must not be killed by a plain launch.
        return;
    }
    const QString running = version.isValid() ? version.value().toString() : QStringLiteral("unknown");
    if (!shouldReplace(running, QStringLiteral(KUICKSHARE_VERSION), force))
    {
        return;
    }
    // The old shell waits for nothing but its own exit now: the background service is only
    // stopped by the core's `Quit` (the tray belongs to it too). Both old pieces go.
    bus.call(QDBusMessage::createMethodCall(shellService, QStringLiteral("/MainApplication"), QStringLiteral("org.qtproject.Qt.QCoreApplication"), QStringLiteral("quit")), QDBus::Block, 2000);
    if (waitGone(shellService, 8000))
    {
        // The shell no longer stops its own core on the way out, so the updater's replacement
        // does it: the new instance starts a fresh one of its own version afterwards.
        auto quit = QDBusMessage::createMethodCall(coreService, QStringLiteral("/io/github/the_beper/KuickShare"), QStringLiteral("io.github.the_beper.KuickShare1"), QStringLiteral("Quit"));
        bus.call(quit, QDBus::Block, 5000);
        waitGone(coreService, 8000);
    }
}
