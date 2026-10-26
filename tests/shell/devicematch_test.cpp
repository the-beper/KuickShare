#include <QtTest>

#include "devicematch.h"

class DeviceMatchTest : public QObject
{
    Q_OBJECT

private:
    const QList<DeviceRef> devices{{QStringLiteral("a"), QStringLiteral("Pixel 7")}, {QStringLiteral("b"), QStringLiteral("Galaxy A55")}};

private Q_SLOTS:
    void matchesTheWholeNameIgnoringCase()
    {
        const DeviceMatch match = matchDevice(devices, QStringLiteral("pIxEl 7"));
        QCOMPARE(match.kind, DeviceMatch::Kind::Unique);
        QCOMPARE(match.id, QStringLiteral("a"));
        QCOMPARE(matchDevice(devices, QStringLiteral("Pixel")).kind, DeviceMatch::Kind::None);
    }

    void trimsBothSides()
    {
        const DeviceMatch match = matchDevice({{QStringLiteral("a"), QStringLiteral(" Pixel 7 ")}}, QStringLiteral("  pixel 7\t"));
        QCOMPARE(match.kind, DeviceMatch::Kind::Unique);
    }

    void noneWhenAbsentOrEmpty()
    {
        QCOMPARE(matchDevice(devices, QStringLiteral("Tablet")).kind, DeviceMatch::Kind::None);
        QCOMPARE(matchDevice({}, QStringLiteral("Pixel 7")).kind, DeviceMatch::Kind::None);
    }

    void ambiguousWhenTwoMatch()
    {
        const QList<DeviceRef> twins{{QStringLiteral("a"), QStringLiteral("Pixel 7")}, {QStringLiteral("b"), QStringLiteral("PIXEL 7")}, {QStringLiteral("c"), QStringLiteral("Other")}};
        const DeviceMatch match = matchDevice(twins, QStringLiteral("Pixel 7"));
        QCOMPARE(match.kind, DeviceMatch::Kind::Ambiguous);
        QVERIFY(match.id.isEmpty());
    }
};

QTEST_APPLESS_MAIN(DeviceMatchTest)
#include "devicematch_test.moc"
