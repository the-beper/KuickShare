#pragma once

#include <KNotification>
#include <KUiServerV2JobTracker>
#include <QHash>
#include <QObject>
#include <QElapsedTimer>
#include <QPointer>
#include <QVariantMap>

class CoreBridge;
class TransferJob;

/// Turns transfer events into Plasma notifications (consent, results) and job progress.
class TransferWatcher : public QObject
{
    Q_OBJECT

public:
    explicit TransferWatcher(CoreBridge *core, QObject *parent = nullptr);

Q_SIGNALS:
    // The user clicked "View" on a text or link notification.
    void viewRequested(const QString &id);

private:
    struct Tracked
    {
        QPointer<TransferJob> job;
        QPointer<KNotification> consent;
        quint64 consentToken = 0; // the core's token of the consent the prompt answers
        bool noTrust = false;     // the core could not trust this sender: the prompt does not offer it again
        bool finished = false;
        bool started = false; // data started to move
        qint64 sampleMs = 0;
        qulonglong sampleBytes = 0;
        double speed = 0; // bytes per second, smoothed
    };

    void onTransferChanged(const QString &id);
    void onSendRequested(const QString &id, const QString &device, int files);
    void onSendRefused(const QString &id);
    /// The core did not take an answer: the prompt is stale.
    void onAnswerRefused(const QString &id);
    /// The core did not take the answer but the transfer still waits: show the prompt again.
    void onAnswerNotApplied(const QString &id);
    /// The core went away or restarted: no prompt may answer anything of the new session.
    void closeConsents();
    void notifyConsent(const QString &id, const QString &device, const QString &pin, quint64 token, Tracked &tracked);
    void notifyReceived(const QString &id, const QString &device, const QVariantMap &info, bool isText);
    void notifyProblem(const QString &eventId, const QString &title, const QString &text);

    CoreBridge *m_core;
    KUiServerV2JobTracker m_tracker;
    QElapsedTimer m_clock;
    QHash<QString, Tracked> m_tracked;
};
