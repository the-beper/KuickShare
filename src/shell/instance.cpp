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

namespace
{
    // The core is headless and outlives its shell: after an update a core of the old version may
    // be there with no shell. It goes unless a transfer is in flight (that one is not cut), and
    // the new shell starts a fresh one. A core that does not answer (busy) is left alone.
    void replaceStaleCore(QDBusConnection &bus, bool force)
    {
        if (!registered(coreService))
        {
            return;
        }
        const QString path = QStringLiteral("/io/github/the_beper/KuickShare");
        const QString interfaceName = QStringLiteral("io.github.the_beper.KuickShare1");
        auto read = [&](const QString &property)
        {
            auto get = QDBusMessage::createMethodCall(coreService, path, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("Get"));
            get << interfaceName << property;
            return QDBusReply<QVariant>(bus.call(get, QDBus::Block, 2000));
        };
        const QDBusReply<QVariant> version = read(QStringLiteral("Version"));
        if (!version.isValid() || (!force && version.value().toString() == QStringLiteral(KUICKSHARE_VERSION)))
        {
            return;
        }
        const QDBusReply<QVariant> busy = read(QStringLiteral("TransferBusy"));
        if (!busy.isValid() || busy.value().toBool())
        {
            return;
        }
        bus.call(QDBusMessage::createMethodCall(coreService, path, interfaceName, QStringLiteral("Quit")), QDBus::Block, 5000);
        waitGone(coreService, 8000);
    }
}

void Instance::replaceRunning(bool force)
{
    auto bus = QDBusConnection::sessionBus();
    if (!bus.isConnected())
    {
        return;
    }
    if (!registered(shellService))
    {
        replaceStaleCore(bus, force);
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
    // The shell does not stop the core when it exits; the core stops only on its own `Quit`
    // (the tray belongs to it). Both old pieces go.
    bus.call(QDBusMessage::createMethodCall(shellService, QStringLiteral("/MainApplication"), QStringLiteral("org.qtproject.Qt.QCoreApplication"), QStringLiteral("quit")), QDBus::Block, 2000);
    if (waitGone(shellService, 8000))
    {
        // The new instance starts a fresh core of its own version afterwards.
        auto quit = QDBusMessage::createMethodCall(coreService, QStringLiteral("/io/github/the_beper/KuickShare"), QStringLiteral("io.github.the_beper.KuickShare1"), QStringLiteral("Quit"));
        bus.call(quit, QDBus::Block, 5000);
        waitGone(coreService, 8000);
    }
}
