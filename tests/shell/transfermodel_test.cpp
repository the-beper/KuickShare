#include <QtTest>

#include "transfermodel.h"

class TransferModelTest : public QObject
{
    Q_OBJECT

private:
    static QVariantMap event(const QString &state, const QString &device = {})
    {
        return {{QStringLiteral("state"), state}, {QStringLiteral("device"), device}};
    }
    static QString stateAt(const TransferModel &model, int row)
    {
        return model.data(model.index(row), TransferModel::StateRole).toString();
    }

private Q_SLOTS:
    void updatesMergeAndKeepKnownFields()
    {
        TransferModel model;
        model.update(QStringLiteral("a"), event(QStringLiteral("SendingFiles"), QStringLiteral("Pixel")));
        model.update(QStringLiteral("a"), event(QStringLiteral("SendingFiles"))); // device now empty
        QCOMPARE(model.rowCount(), 1);
        QCOMPARE(model.info(QStringLiteral("a")).value(QStringLiteral("device")).toString(), QStringLiteral("Pixel"));
    }

    void finishedTransferIsRestartedAsNewRow()
    {
        TransferModel model;
        model.update(QStringLiteral("a"), event(QStringLiteral("Finished")));
        model.update(QStringLiteral("a"), event(QStringLiteral("SendingFiles"))); // same id, new transfer
        QCOMPARE(model.rowCount(), 2);
        QCOMPARE(stateAt(model, 0), QStringLiteral("SendingFiles"));
        QCOMPARE(stateAt(model, 1), QStringLiteral("Finished"));
        QCOMPARE(model.info(QStringLiteral("a")).value(QStringLiteral("state")).toString(), QStringLiteral("SendingFiles"));
    }

    void pendingSendIsTakenOverByTheFirstEvent()
    {
        TransferModel model;
        model.update(QStringLiteral("ble:X"), {{QStringLiteral("kind"), QStringLiteral("outbound")},
                                               {QStringLiteral("state"), QStringLiteral("Starting")},
                                               {QStringLiteral("device"), QStringLiteral("Pixel")}});
        model.update(QStringLiteral("ble:X"), event(QStringLiteral("SentIntroduction")));
        QCOMPARE(model.rowCount(), 1);
        QCOMPARE(stateAt(model, 0), QStringLiteral("SentIntroduction"));
        QCOMPARE(model.data(model.index(0), TransferModel::KindRole).toString(), QStringLiteral("outbound"));
        QCOMPARE(model.data(model.index(0), TransferModel::DeviceRole).toString(), QStringLiteral("Pixel"));
    }

    void refusedPendingSendEndsAsOneFinishedRow()
    {
        TransferModel model;
        model.update(QStringLiteral("a"), event(QStringLiteral("Starting"), QStringLiteral("Pixel")));
        model.update(QStringLiteral("a"), event(QStringLiteral("Disconnected")));
        model.update(QStringLiteral("a"), event(QStringLiteral("Disconnected"))); // the watchdog later
        QCOMPARE(model.rowCount(), 1);
        QVERIFY(TransferModel::isFinal(stateAt(model, 0)));
        model.update(QStringLiteral("a"), event(QStringLiteral("Starting"), QStringLiteral("Pixel"))); // a retry
        QCOMPARE(model.rowCount(), 2);
    }

    void cancellingAPendingSendEndsItAsCancelled()
    {
        TransferModel model;
        model.beginPending(QStringLiteral("a"), {{QStringLiteral("device"), QStringLiteral("Pixel")}});
        QVERIFY(model.cancelPending(QStringLiteral("a")));
        QCOMPARE(stateAt(model, 0), QStringLiteral("Cancelled"));
        QVERIFY(!model.cancelPending(QStringLiteral("a"))); // no longer Starting
        model.update(QStringLiteral("b"), event(QStringLiteral("SendingFiles")));
        QVERIFY(!model.cancelPending(QStringLiteral("b")));
        QCOMPARE(model.rowCount(), 2);
    }

    void anOlderSendFailureDoesNotEndANewerSend()
    {
        TransferModel model;
        const quint64 first = model.beginPending(QStringLiteral("a"), {{QStringLiteral("device"), QStringLiteral("Pixel")}});
        model.failPending(QStringLiteral("a"), first);
        QCOMPARE(stateAt(model, 0), QStringLiteral("Disconnected"));
        const quint64 second = model.beginPending(QStringLiteral("a"), {{QStringLiteral("device"), QStringLiteral("Pixel")}});
        QCOMPARE(model.rowCount(), 2);
        QVERIFY(second != first);
        model.failPending(QStringLiteral("a"), first); // the timer of the first send fires late
        QCOMPARE(stateAt(model, 0), QStringLiteral("Starting"));
        model.failPending(QStringLiteral("a"), second);
        QCOMPARE(stateAt(model, 0), QStringLiteral("Disconnected"));
    }

    void failurePendingIgnoresASendTheCoreHasTakenOver()
    {
        TransferModel model;
        const quint64 number = model.beginPending(QStringLiteral("a"), {});
        model.update(QStringLiteral("a"), event(QStringLiteral("SentIntroduction")));
        model.failPending(QStringLiteral("a"), number);
        QCOMPARE(stateAt(model, 0), QStringLiteral("SentIntroduction"));
    }

    void busyFollowsTheLiveTransfers()
    {
        TransferModel model;
        QSignalSpy spy(&model, &TransferModel::busyChanged);
        QVERIFY(!model.busy());
        model.beginPending(QStringLiteral("a"), {});
        QVERIFY(model.busy());
        QCOMPARE(spy.count(), 1);
        model.update(QStringLiteral("a"), event(QStringLiteral("SendingFiles")));
        QCOMPARE(spy.count(), 1); // still busy, no new signal
        model.update(QStringLiteral("a"), event(QStringLiteral("Finished")));
        QVERIFY(!model.busy());
        QCOMPARE(spy.count(), 2);
    }

    void aRequestWaitingForApprovalIsNotBusy()
    {
        TransferModel model;
        model.update(QStringLiteral("in"), {{QStringLiteral("kind"), QStringLiteral("inbound")}, {QStringLiteral("state"), QStringLiteral("WaitingForUserConsent")}});
        QVERIFY(!model.busy());
        model.update(QStringLiteral("in"), {{QStringLiteral("state"), QStringLiteral("ReceivingFiles")}});
        QVERIFY(model.busy());
        model.clearAll();
        QVERIFY(!model.busy());
    }

    void transferEndedIsEmittedOncePerTransfer()
    {
        TransferModel model;
        QSignalSpy spy(&model, &TransferModel::transferEnded);
        model.update(QStringLiteral("a"), event(QStringLiteral("SendingFiles")));
        QCOMPARE(spy.count(), 0);
        model.update(QStringLiteral("a"), event(QStringLiteral("Finished")));
        model.update(QStringLiteral("a"), event(QStringLiteral("Disconnected"))); // the second final state is ignored
        QCOMPARE(spy.count(), 1);
        QCOMPARE(spy.first().at(1).toString(), QStringLiteral("Finished"));
        model.addHistory(event(QStringLiteral("Finished"))); // the saved history is not news
        QCOMPARE(spy.count(), 1);
        const quint64 number = model.beginPending(QStringLiteral("b"), {});
        model.failPending(QStringLiteral("b"), number);
        QCOMPARE(spy.count(), 2);
        QCOMPARE(spy.last().at(1).toString(), QStringLiteral("Disconnected"));
    }

    void historyIsAppendedBelowLiveTransfers()
    {
        TransferModel model;
        model.addHistory(event(QStringLiteral("Finished"), QStringLiteral("old")));
        model.update(QStringLiteral("live"), event(QStringLiteral("SendingFiles"), QStringLiteral("now")));
        QCOMPARE(model.rowCount(), 2);
        QCOMPARE(stateAt(model, 0), QStringLiteral("SendingFiles"));
        QCOMPARE(stateAt(model, 1), QStringLiteral("Finished"));
    }

    void clearFinishedKeepsActiveTransfers()
    {
        TransferModel model;
        model.addHistory(event(QStringLiteral("Finished")));
        model.addHistory(event(QStringLiteral("Disconnected")));
        model.update(QStringLiteral("live"), event(QStringLiteral("ReceivingFiles")));
        model.clearFinished();
        QCOMPARE(model.rowCount(), 1);
        QCOMPARE(stateAt(model, 0), QStringLiteral("ReceivingFiles"));
    }

    void filesRoleSplitsOnNewlines()
    {
        TransferModel model;
        QVariantMap info = event(QStringLiteral("Finished"));
        info.insert(QStringLiteral("files"), QStringLiteral("a.mp4\nb.pdf"));
        model.addHistory(info);
        QCOMPARE(model.data(model.index(0), TransferModel::FilesRole).toStringList(), (QStringList{QStringLiteral("a.mp4"), QStringLiteral("b.pdf")}));
    }

    void onlySendingOrReceivingCountsAsActive()
    {
        TransferModel model;
        QVERIFY(!model.hasActive());
        model.update(QStringLiteral("a"), event(QStringLiteral("WaitingForUserConsent")));
        QVERIFY(!model.hasActive()); // waiting for the user is not a transfer in progress
        model.update(QStringLiteral("b"), event(QStringLiteral("ReceivingFiles")));
        QVERIFY(model.hasActive());
        model.update(QStringLiteral("b"), event(QStringLiteral("Finished")));
        QVERIFY(!model.hasActive());
        model.update(QStringLiteral("c"), event(QStringLiteral("SendingFiles")));
        QVERIFY(model.hasActive());
    }

    void removeDeletesOneRowAndIgnoresBadRows()
    {
        TransferModel model;
        model.update(QStringLiteral("a"), event(QStringLiteral("Finished"), QStringLiteral("Pixel")));
        model.update(QStringLiteral("b"), event(QStringLiteral("Finished"), QStringLiteral("Tab")));
        model.remove(5);
        model.remove(-1);
        QCOMPARE(model.rowCount(), 2);
        model.remove(1); // "a", the older row
        QCOMPARE(model.rowCount(), 1);
        QCOMPARE(model.data(model.index(0), TransferModel::DeviceRole).toString(), QStringLiteral("Tab"));
    }

    void removeTakesTheRowEvenWhenIdsRepeat()
    {
        TransferModel model;
        model.update(QStringLiteral("a"), event(QStringLiteral("Finished"), QStringLiteral("first")));
        model.update(QStringLiteral("a"), event(QStringLiteral("Starting"), QStringLiteral("second")));
        model.update(QStringLiteral("a"), event(QStringLiteral("Finished")));
        QCOMPARE(model.rowCount(), 2);
        model.remove(1); // the older one
        QCOMPARE(model.rowCount(), 1);
        QCOMPARE(model.data(model.index(0), TransferModel::DeviceRole).toString(), QStringLiteral("second"));
    }

    void removeHistoryFindsTheRowByUid()
    {
        TransferModel model;
        model.addHistory({{QStringLiteral("state"), QStringLiteral("Finished")}, {QStringLiteral("history_uid"), 7}});
        model.addHistory({{QStringLiteral("state"), QStringLiteral("Finished")}, {QStringLiteral("history_uid"), 9}});
        model.removeHistory(8);
        QCOMPARE(model.rowCount(), 2);
        model.removeHistory(7);
        QCOMPARE(model.rowCount(), 1);
        QCOMPARE(model.data(model.index(0), TransferModel::HistoryUidRole).toInt(), 9);
    }

    void disconnectLiveEndsEveryNonFinalRow()
    {
        TransferModel model;
        QSignalSpy ended(&model, &TransferModel::transferEnded);
        model.update(QStringLiteral("old"), event(QStringLiteral("Finished")));
        model.update(QStringLiteral("old"), event(QStringLiteral("SendingFiles"))); // a newer transfer, same id
        model.update(QStringLiteral("in"), {{QStringLiteral("kind"), QStringLiteral("inbound")}, {QStringLiteral("state"), QStringLiteral("WaitingForUserConsent")}});
        model.beginPending(QStringLiteral("p"), {});
        QVERIFY(model.hasActive());
        QVERIFY(model.hasLive());
        QVERIFY(model.busy());
        ended.clear(); // the first Finished update already ended a row
        QStringList ids = model.disconnectLive();
        ids.sort();
        QCOMPARE(ids, (QStringList{QStringLiteral("in"), QStringLiteral("old"), QStringLiteral("p")}));
        QCOMPARE(model.rowCount(), 4);
        for (int row = 0; row < model.rowCount(); ++row) {
            QVERIFY(TransferModel::isFinal(stateAt(model, row)));
        }
        QCOMPARE(ended.count(), 3);
        QVERIFY(!model.hasActive());
        QVERIFY(!model.hasLive());
        QVERIFY(!model.busy());
        QVERIFY(model.disconnectLive().isEmpty());
    }

    void hasLiveCountsAnApprovalRequest()
    {
        TransferModel model;
        QVERIFY(!model.hasLive());
        model.update(QStringLiteral("in"), {{QStringLiteral("kind"), QStringLiteral("inbound")}, {QStringLiteral("state"), QStringLiteral("WaitingForUserConsent")}});
        QVERIFY(!model.hasActive());
        QVERIFY(model.hasLive());
    }

    void failPendingSaysWhetherItEndedTheRow()
    {
        TransferModel model;
        const quint64 number = model.beginPending(QStringLiteral("a"), {});
        QVERIFY(!model.failPending(QStringLiteral("a"), number + 1));
        QVERIFY(model.failPending(QStringLiteral("a"), number));
        QVERIFY(!model.failPending(QStringLiteral("a"), number)); // already ended
    }

    void secondFinalStateIsIgnored()
    {
        TransferModel model;
        model.update(QStringLiteral("a"), event(QStringLiteral("Finished")));
        model.update(QStringLiteral("a"), event(QStringLiteral("Disconnected")));
        QCOMPARE(model.rowCount(), 1);
        QCOMPARE(stateAt(model, 0), QStringLiteral("Finished"));
    }
};

QTEST_APPLESS_MAIN(TransferModelTest)
#include "transfermodel_test.moc"
