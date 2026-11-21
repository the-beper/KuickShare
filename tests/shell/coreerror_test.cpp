#include <QtTest>

#include <QDBusMessage>

#include "coreerror.h"

using CoreError::Kind;

namespace
{
    QDBusError named(const QString &name)
    {
        return QDBusError(QDBusMessage::createError(name, QStringLiteral("anyhow: cannot create /x: Permission denied")));
    }
}

class CoreErrorTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void nobodyAnsweringIsUnreachable()
    {
        for (const auto type : {QDBusError::ServiceUnknown, QDBusError::NoReply, QDBusError::TimedOut, QDBusError::Disconnected, QDBusError::UnknownObject})
        {
            QCOMPARE(CoreError::kindOf(QDBusError(type, QStringLiteral("x"))), Kind::Unreachable);
        }
    }

    void theCoreNamingItsEngineStoppedIsNotRunning()
    {
        QCOMPARE(CoreError::kindOf(named(QStringLiteral("io.github.the_beper.KuickShare1.Error.NotRunning"))), Kind::NotRunning);
    }

    void everythingElseIsOther()
    {
        QCOMPARE(CoreError::kindOf(named(QStringLiteral("org.freedesktop.DBus.Error.Failed"))), Kind::Other);
        QCOMPARE(CoreError::kindOf(named(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"))), Kind::Other);
        QCOMPARE(CoreError::kindOf(named(QStringLiteral("io.github.the_beper.KuickShare1.Error.DeviceChanged"))), Kind::Other);
    }
};

QTEST_APPLESS_MAIN(CoreErrorTest)
#include "coreerror_test.moc"
