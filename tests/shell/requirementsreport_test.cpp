#include <QtTest>

#include "requirementsreport.h"

using namespace RequirementsReport;

namespace
{
    QByteArray row(const QString &id, const QString &status = QStringLiteral("ready"), const QString &reason = QStringLiteral("ok"), const QString &extra = QString())
    {
        QString json = QStringLiteral(R"({"id":"%1","status":"%2","category":"critical","reason_code":"%3","details":"d","device":null,"verification":"v","affects":["sending"])")
                           .arg(id, status, reason);
        if (!extra.isEmpty())
        {
            json += QLatin1Char(',') + extra;
        }
        return json.toUtf8() + '}';
    }

    QByteArray report(const QList<QByteArray> &rows, qint64 generatedAt = 1793260800000, const QByteArray &schema = "1", const QByteArray &caps = R"({"sending":"available","receiving":"unknown","join_network":"degraded"})")
    {
        return "{\"schema_version\":" + schema + ",\"generated_at_ms\":" + QByteArray::number(generatedAt) + ",\"ttl_ms\":30000,\"packaging\":\"native\",\"core_version\":\"1.4.1\",\"capabilities\":" + caps + ",\"requirements\":[" + rows.join(',') + "]}";
    }
} // namespace

class RequirementsReportTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void aValidReportIsParsed()
    {
        const Parsed p = parse(report({row(QStringLiteral("nm.service")), row(QStringLiteral("bluez.service"), QStringLiteral("blocked"), QStringLiteral("not_running"))}));
        QVERIFY(p.ok());
        QCOMPARE(p.generatedAtMs, qint64(1793260800000));
        QCOMPARE(p.ttlMs, qint64(30000));
        QCOMPARE(p.packaging, QStringLiteral("native"));
        QCOMPARE(p.coreVersion, QStringLiteral("1.4.1"));
        QCOMPARE(p.sending, Capability::Available);
        QCOMPARE(p.receiving, Capability::Unknown);
        QCOMPARE(p.joinNetwork, Capability::Degraded);
        QCOMPARE(p.rows.size(), 2);
        QCOMPARE(p.rows.at(1).id, QStringLiteral("bluez.service"));
        QCOMPARE(p.rows.at(1).status, Status::Blocked);
        QCOMPARE(p.rows.at(1).category, Category::Critical);
        QCOMPARE(p.rows.at(1).reasonCode, QStringLiteral("not_running"));
        QCOMPARE(p.rows.at(1).affects, QStringList{QStringLiteral("sending")});
    }

    void invalidJsonIsAnError_data()
    {
        QTest::addColumn<QByteArray>("input");
        QTest::newRow("empty") << QByteArray();
        QTest::newRow("not json") << QByteArray("not json at all");
        QTest::newRow("truncated") << report({row(QStringLiteral("a"))}).left(40);
        QTest::newRow("array") << QByteArray("[1,2,3]");
        QTest::newRow("string") << QByteArray("\"x\"");
        QTest::newRow("binary") << QByteArray("\xff\xfe\x00\x01", 4);
        QTest::newRow("trailing garbage") << (report({}) + "}}");
    }

    void invalidJsonIsAnError()
    {
        QFETCH(QByteArray, input);
        const Parsed p = parse(input);
        QVERIFY(!p.ok());
        QVERIFY(p.rows.isEmpty());
    }

    void aSchemaVersionOtherThanOneIsRefused_data()
    {
        QTest::addColumn<QByteArray>("schema");
        QTest::newRow("zero") << QByteArray("0");
        QTest::newRow("two") << QByteArray("2");
        QTest::newRow("negative") << QByteArray("-1");
        QTest::newRow("string") << QByteArray("\"1\"");
        QTest::newRow("null") << QByteArray("null");
        QTest::newRow("float") << QByteArray("1.5");
    }

    void aSchemaVersionOtherThanOneIsRefused()
    {
        QFETCH(QByteArray, schema);
        const Parsed p = parse(report({row(QStringLiteral("a"))}, 1000, schema));
        QVERIFY(!p.ok());
        QCOMPARE(p.error, Error::Schema);
        QVERIFY(p.rows.isEmpty());
    }

    void aMissingSchemaVersionOrTimeIsRefused()
    {
        QCOMPARE(parse(R"({"generated_at_ms":1,"requirements":[]})").error, Error::Schema);
        QCOMPARE(parse(R"({"schema_version":1,"requirements":[]})").error, Error::Schema);
        QCOMPARE(parse(R"({"schema_version":1,"generated_at_ms":1})").error, Error::Schema);
    }

    void oversizeInputIsRefusedWithoutParsing()
    {
        QByteArray big = report({row(QStringLiteral("a"))});
        big.insert(big.size() - 2, QByteArray(kMaxBytes, ' '));
        QVERIFY(big.size() > kMaxBytes);
        const Parsed p = parse(big);
        QCOMPARE(p.error, Error::Limits);
        QVERIFY(p.rows.isEmpty());
        // Exactly at the limit is fine.
        QByteArray fit = report({row(QStringLiteral("a"))});
        fit.insert(fit.size() - 2, QByteArray(kMaxBytes - fit.size(), ' '));
        QCOMPARE(fit.size(), kMaxBytes);
        QVERIFY(parse(fit).ok());
    }

    void tooManyRowsAreRefused()
    {
        QList<QByteArray> rows;
        for (int i = 0; i < kMaxRows; ++i)
        {
            rows << row(QStringLiteral("r%1").arg(i));
        }
        const Parsed atLimit = parse(report(rows));
        QVERIFY(atLimit.ok());
        QCOMPARE(atLimit.rows.size(), kMaxRows);

        rows << row(QStringLiteral("one.too.many"));
        const Parsed p = parse(report(rows));
        QCOMPARE(p.error, Error::Limits);
        QVERIFY(p.rows.isEmpty());
    }

    void overLongFieldsAreCut()
    {
        const QString longText(5000, QLatin1Char('x'));
        const QString extra = QStringLiteral(R"("details":"%1","device":"%1","verification":"%1")").arg(longText);
        const QByteArray json = report({QStringLiteral(R"({"id":"nm.service","status":"ready","category":"critical","reason_code":"ok",%1})").arg(extra).toUtf8()});
        const Parsed p = parse(json);
        QVERIFY(p.ok());
        QCOMPARE(p.rows.size(), 1);
        QVERIFY(p.rows.at(0).details.size() <= kMaxDetails);
        QVERIFY(p.rows.at(0).device.size() <= kMaxDevice);
        QVERIFY(p.rows.at(0).verification.size() <= kMaxVerification);
        QVERIFY(!p.rows.at(0).details.isEmpty());

        // An over-long id or reason code is cut to a bounded token, never kept whole.
        const QString longId(500, QLatin1Char('a'));
        const Parsed q = parse(report({row(longId, QStringLiteral("ready"), longId)}));
        QVERIFY(q.ok());
        QVERIFY(q.rows.at(0).id.size() <= kMaxToken);
        QVERIFY(q.rows.at(0).reasonCode.size() <= kMaxToken);
    }

    void unknownEnumStringsBecomeUnknown()
    {
        const Parsed p = parse(report({row(QStringLiteral("a"), QStringLiteral("bogus")),
                                       row(QStringLiteral("b"), QStringLiteral("READY")),
                                       row(QStringLiteral("c"), QStringLiteral("")),
                                       QByteArray(R"({"id":"d","status":42,"category":"nonsense","reason_code":"x"})"),
                                       QByteArray(R"({"id":"e"})")},
                                      1000, "1", R"({"sending":"fantastic","receiving":7,"join_network":null})"));
        QVERIFY(p.ok());
        QCOMPARE(p.rows.size(), 5);
        for (const Row &r : p.rows)
        {
            QCOMPARE(r.status, Status::Unknown);
        }
        QCOMPARE(p.rows.at(3).category, Category::Unknown);
        QCOMPARE(p.sending, Capability::Unknown);
        QCOMPARE(p.receiving, Capability::Unknown);
        QCOMPARE(p.joinNetwork, Capability::Unknown);
        // A row the new core knows how to say but this shell does not never turns into "ready" or "blocked".
        QVERIFY(p.rows.at(3).affects.isEmpty());
    }

    void unknownPackagingAndAffectsAreDropped()
    {
        const QByteArray json = "{\"schema_version\":1,\"generated_at_ms\":5,\"packaging\":\"snap\",\"requirements\":[{\"id\":\"a\",\"affects\":[\"sending\",\"sending\",\"teleport\",5]}]}";
        const Parsed p = parse(json);
        QVERIFY(p.ok());
        QCOMPARE(p.packaging, QStringLiteral("unknown"));
        QCOMPARE(p.rows.at(0).affects, QStringList{QStringLiteral("sending")});
    }

    void markupAndControlCharactersInHostTextAreNeutralised()
    {
        const QString evil = QStringLiteral("<b>bold</b><img src=x onerror=1>\\u0000\\u0007\\n\\r\\u202e\\u2028 end");
        const QByteArray json = report({QStringLiteral(R"({"id":"nm.service","status":"ready","category":"critical","reason_code":"ok","details":"%1","device":"%1","verification":"%1","affects":[]})").arg(evil).toUtf8()});
        const Parsed p = parse(json);
        QVERIFY(p.ok());
        const Row &r = p.rows.at(0);
        for (const QString &text : {r.details, r.device, r.verification})
        {
            QVERIFY2(!text.contains(QLatin1Char('<')), qPrintable(text));
            QVERIFY2(!text.contains(QLatin1Char('>')), qPrintable(text));
            for (const QChar c : text)
            {
                QVERIFY2(c.category() != QChar::Other_Control && c.category() != QChar::Other_Format && c.category() != QChar::Separator_Line && c.category() != QChar::Separator_Paragraph, qPrintable(text));
            }
            QVERIFY(text.contains(QStringLiteral("bold")));
        }
        // The helper on its own, and tokens: an id with markup keeps only the identifier characters.
        QVERIFY(!clean(QStringLiteral("a<script>b"), 100).contains(QLatin1Char('<')));
        QCOMPARE(clean(QStringLiteral("  x\ty  "), 100), QStringLiteral("xy"));
        const Parsed q = parse(report({row(QStringLiteral("<b>nm.service</b>"), QStringLiteral("ready"), QStringLiteral("<i>ok</i>"))}));
        QVERIFY(q.ok());
        QVERIFY(!q.rows.at(0).id.contains(QLatin1Char('<')));
        QVERIFY(!q.rows.at(0).reasonCode.contains(QLatin1Char('<')));
    }

    void garbageNeverCrashes()
    {
        const QList<QByteArray> inputs = {
            R"({"schema_version":1,"generated_at_ms":1,"requirements":"no"})",
            R"({"schema_version":1,"generated_at_ms":1,"requirements":[1,"x",null,[],{}]})",
            R"({"schema_version":1,"generated_at_ms":-5,"requirements":[]})",
            R"({"schema_version":1,"generated_at_ms":1e300,"requirements":[]})",
            R"({"schema_version":1,"generated_at_ms":1,"requirements":[{"id":{"a":1}}],"capabilities":[1]})",
        };
        for (const QByteArray &input : inputs)
        {
            const Parsed p = parse(input);
            Q_UNUSED(p);
        }
        QCOMPARE(parse(inputs.at(2)).error, Error::Schema);
        QCOMPARE(parse(inputs.at(3)).error, Error::Schema);
    }

    // ---- stale replies: the tracker is driven by values, no timers and no clock

    void aReplyIsAcceptedOnlyForTheLatestRequest()
    {
        RequestTracker t;
        const auto first = t.begin();
        const auto second = t.begin();
        QVERIFY(!t.isCurrent(first));
        QVERIFY(t.isCurrent(second));
        // An older request answering after a newer one was issued is dropped.
        QVERIFY(!t.accept(first));
        QVERIFY(t.accept(second));
    }

    void aSignalSupersedesTheRequestsInFlight()
    {
        RequestTracker t;
        const auto pending = t.begin();
        QVERIFY(t.acceptSignal());     // a request is in flight; the signal still applies
        QVERIFY(t.isCurrent(pending)); // so the shell can still clear its busy flag
        QVERIFY(!t.accept(pending));   // but the reply is older than the signal
        const auto later = t.begin();
        QVERIFY(t.accept(later));
    }

    void aWallClockThatGoesBackDoesNotBlockNewerReports()
    {
        // The order is local: generated_at_ms (the core's wall clock) is not an input, so a clock set back
        // cannot make a newer reply look old.
        RequestTracker t;
        QVERIFY(t.accept(t.begin())); // generated at 9000 on the old clock
        QVERIFY(t.accept(t.begin())); // generated at 100 after the clock went back
        QVERIFY(t.acceptSignal());
        QVERIFY(t.acceptSignal());
        QVERIFY(t.accept(t.begin()));
        QVERIFY(parse(report({}, 100)).ok()); // and the parser has no objection to such a time
    }

    void aRestartStartsANewEpoch()
    {
        RequestTracker t;
        const auto before = t.begin();
        QVERIFY(t.accept(before));
        const quint64 epoch = t.epoch();

        t.invalidate(); // the core went away and came back
        QVERIFY(t.epoch() > epoch);
        QVERIFY(!t.sameEpoch(before));
        // A late reply from before the restart is never applied.
        QVERIFY(!t.accept(before));
        const auto after = t.begin();
        QCOMPARE(after.epoch, t.epoch());
        QVERIFY(t.accept(after));
        // Several restarts in a row each get their own epoch.
        const quint64 e1 = t.epoch();
        t.invalidate();
        t.invalidate();
        QVERIFY(t.epoch() >= e1 + 2);
    }

    void stalenessIsComputedFromTheTimeWithoutATimer()
    {
        QCOMPARE(staleInMs(1000, 30000, 1000), qint64(30000));
        QCOMPARE(staleInMs(1000, 30000, 11000), qint64(20000));
        QCOMPARE(staleInMs(1000, 30000, 31000), qint64(0));
        QCOMPARE(staleInMs(1000, 30000, 99000), qint64(0));
        // A report dated in the future counts as just generated.
        QCOMPARE(staleInMs(50000, 30000, 1000), qint64(30000));
    }

    // ---- the copied diagnostic report

    void theCopiedReportLeavesOutDetailsAndDevice()
    {
        const QByteArray json = report({QStringLiteral(R"({"id":"nm.device","status":"warning","category":"critical","reason_code":"ok_first_of_many","details":"SECRET-DETAILS iface wlan0 aa:bb:cc:dd:ee:ff","device":"SECRET-WLAN","verification":"GetDevices without interaction","affects":["sending"]})").toUtf8(),
                                        row(QStringLiteral("bluez.service"), QStringLiteral("blocked"), QStringLiteral("not_running"))});
        const Parsed p = parse(json);
        QVERIFY(p.ok());
        const QString text = formatReport(p, QStringLiteral("1.4.1"), true);

        QVERIFY(text.startsWith(QStringLiteral("KuickShare diagnostic report\n")));
        QVERIFY(text.contains(QStringLiteral("schema 1")));
        QVERIFY(text.contains(QStringLiteral("packaging native")));
        QVERIFY(text.contains(QStringLiteral("stale: yes")));
        QVERIFY(text.contains(QStringLiteral("Sending available | Receiving unknown | Join network degraded")));
        QVERIFY(text.contains(QStringLiteral("nm.device warning ok_first_of_many [critical] GetDevices without interaction")));
        QVERIFY(text.contains(QStringLiteral("bluez.service blocked not_running [critical]")));
        // Privacy: nothing the host or the network supplied is copied.
        QVERIFY(!text.contains(QStringLiteral("SECRET")));
        QVERIFY(!text.contains(QStringLiteral("wlan0")));
        QVERIFY(!text.contains(QStringLiteral("aa:bb")));
        QVERIFY(!text.contains(QLatin1Char('<')));
        for (const QString &line : text.split(QLatin1Char('\n')))
        {
            QVERIFY2(line.size() <= kMaxReportLine, qPrintable(line));
        }
        QVERIFY(formatReport(p, QStringLiteral("1.4.1"), false).contains(QStringLiteral("stale: no")));
    }

    void theCopiedReportIsPlainTextEvenWithAHostileShellVersionOrRows()
    {
        const Parsed p = parse(report({row(QStringLiteral("a"), QStringLiteral("ready"), QStringLiteral("ok"), QStringLiteral(R"("verification":"<b>x</b>\n\nINJECTED LINE")"))}));
        QVERIFY(p.ok());
        const QString text = formatReport(p, QStringLiteral("<script>\n1.4.1"), false);
        QVERIFY(!text.contains(QLatin1Char('<')));
        QVERIFY(!text.contains(QLatin1Char('\r')));
        // Every line is either the fixed header or a row: a verification text cannot add a line.
        QCOMPARE(text.split(QLatin1Char('\n'), Qt::SkipEmptyParts).size(), 5);
    }

    void aReportThatCouldNotBeReadIsNotDumped()
    {
        const Parsed p = parse("not json");
        const QString text = formatReport(p, QStringLiteral("1.4.1"), true);
        QVERIFY(text.startsWith(QStringLiteral("KuickShare diagnostic report")));
        QVERIFY(!text.contains(QStringLiteral("not json")));
    }
};

QTEST_APPLESS_MAIN(RequirementsReportTest)
#include "requirementsreport_test.moc"
