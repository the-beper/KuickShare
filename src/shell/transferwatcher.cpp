#include "transferwatcher.h"

#include <QClipboard>
#include <QDesktopServices>
#include <QGuiApplication>
#include <QRegularExpression>
#include <QStandardPaths>
#include <QUrl>

#include <KLocalizedString>

#include "corebridge.h"
#include "transferjob.h"

namespace
{
    bool isTransferring(const QString &state)
    {
        return state == QLatin1String("ReceivingFiles") || state == QLatin1String("SendingFiles");
    }

    QString shortened(const QString &text)
    {
        return text.size() > 200 ? text.left(200) + QStringLiteral("…") : text;
    }
}

TransferWatcher::TransferWatcher(CoreBridge *core, QObject *parent)
    : QObject(parent), m_core(core), m_tracker(this)
{
    m_clock.start();
    connect(core, &CoreBridge::transferChanged, this, &TransferWatcher::onTransferChanged);
    connect(core, &CoreBridge::sendRequested, this, &TransferWatcher::onSendRequested);
    connect(core, &CoreBridge::sendRefused, this, &TransferWatcher::onSendRefused);
    connect(core, &CoreBridge::answerRefused, this, &TransferWatcher::onAnswerRefused);
    connect(core, &CoreBridge::answerNotApplied, this, &TransferWatcher::onAnswerNotApplied);
    // A prompt outlives neither the core it was shown for nor the transfers that core knew.
    connect(core, &CoreBridge::connectedChanged, this, &TransferWatcher::closeConsents);
    connect(core, &CoreBridge::transfersReset, this, &TransferWatcher::closeConsents);
}

void TransferWatcher::onAnswerRefused(const QString &id)
{
    const auto it = m_tracked.find(id);
    if (it != m_tracked.end() && it->consent)
    {
        it->consent->close();
        it->consent.clear();
        it->consentToken = 0;
    }
}

void TransferWatcher::onAnswerNotApplied(const QString &id)
{
    const auto it = m_tracked.find(id);
    if (it == m_tracked.end())
    {
        return;
    }
    // The click closed the notification, but the transfer still waits (it would be declined at the
    // time limit): ask again, without the choice that failed.
    it->consent.clear();
    it->noTrust = true;
    onTransferChanged(id);
}

void TransferWatcher::closeConsents()
{
    for (auto it = m_tracked.begin(); it != m_tracked.end();)
    {
        if (it->consent)
        {
            it->consent->close();
        }
        it->consent.clear();
        it->consentToken = 0;
        // The ids may come back for other transfers: what is finished is forgotten.
        if (it->finished)
        {
            it = m_tracked.erase(it);
        }
        else
        {
            ++it;
        }
    }
}

void TransferWatcher::onSendRequested(const QString &id, const QString &device, int files)
{
    Tracked &tracked = m_tracked[id];
    if (tracked.finished)
    {
        tracked = Tracked(); // the same id starts a new transfer
    }
    if (tracked.job)
    {
        return;
    }
    // Show the job right away: finding the device and connecting can take several seconds.
    auto *job = new TransferJob(id, true, device, this);
    connect(job, &TransferJob::cancelRequested, m_core, &CoreBridge::cancel);
    job->setSummary(i18np("%1 file", "%1 files", files));
    job->setStatus(id.startsWith(QLatin1String("ble:")) ? i18n("Looking for the device…") : i18n("Connecting…"), true);
    m_tracker.registerJob(job);
    job->start();
    tracked.job = job;
}

void TransferWatcher::onSendRefused(const QString &id)
{
    Tracked &tracked = m_tracked[id];
    if (tracked.job && !tracked.started)
    {
        tracked.job->finish(false, true);
    }
    tracked.finished = true;
}

void TransferWatcher::onTransferChanged(const QString &id)
{
    const QVariantMap info = m_core->transfers()->info(id);
    const QString state = info.value(QStringLiteral("state")).toString();
    const bool outbound = info.value(QStringLiteral("kind")).toString() == QLatin1String("outbound");
    const QString device = info.value(QStringLiteral("device")).toString();
    const QString preview = info.value(QStringLiteral("preview")).toString();
    // Text and links have no files; the preview alone can be empty, so the payload decides.
    const QString payloadKind = info.value(QStringLiteral("payload_kind")).toString();
    const bool isText = payloadKind == QLatin1String("Text") || payloadKind == QLatin1String("Url") || payloadKind == QLatin1String("WiFi")
                        || (payloadKind.isEmpty() && info.value(QStringLiteral("files")).toString().isEmpty()
                            && (!preview.isEmpty() || !info.value(QStringLiteral("text")).toString().isEmpty()));
    const qulonglong total = info.value(QStringLiteral("total_bytes")).toULongLong();
    const qulonglong acked = info.value(QStringLiteral("ack_bytes")).toULongLong();

    Tracked &tracked = m_tracked[id];
    if (tracked.finished)
    {
        if (TransferModel::isFinal(state))
        {
            return;
        }
        tracked = Tracked(); // the same id starts a new transfer
    }

    // A paired computer announced this transfer: the core accepts it by itself.
    const bool autoAccepted = !info.value(QStringLiteral("trusted")).toString().isEmpty();
    // The core's token of the consent that waits for an answer; 0 when none does (it was answered).
    const quint64 consentToken = info.value(QStringLiteral("consent")).toULongLong();
    const bool awaitingConsent = !outbound && state == QLatin1String("WaitingForUserConsent") && !autoAccepted && consentToken != 0;
    if (awaitingConsent)
    {
        // A prompt of an older consent (the id was reused) answers nothing now: replace it.
        if (tracked.consent && tracked.consentToken != consentToken)
        {
            tracked.consent->close();
            tracked.consent.clear();
        }
        if (!tracked.consent)
        {
            notifyConsent(id, device, info.value(QStringLiteral("pin")).toString(), consentToken, tracked);
        }
    }
    else if (tracked.consent)
    {
        tracked.consent->close();
        tracked.consent.clear();
        tracked.consentToken = 0;
    }

    // The step a send is in until the files start to move.
    if (outbound && tracked.job && !tracked.started && !TransferModel::isFinal(state))
    {
        tracked.job->setStatus(state == QLatin1String("SentIntroduction") ? i18n("Waiting for the other device to accept") : i18n("Connecting…"), true);
    }

    // Text and links are tiny: only real file transfers get a progress job.
    if (isTransferring(state))
    {
        tracked.started = true;
    }
    if (isTransferring(state) && total > 0 && !isText)
    {
        if (tracked.job)
        {
            // Over Bluetooth the phone offers a Wi-Fi link right after it accepted: nothing moves
            // until it is up, which takes several seconds.
            if (outbound && acked == 0 && id.startsWith(QLatin1String("ble:")))
            {
                tracked.job->setStatus(i18n("Switching to Wi-Fi…"), true);
            }
            else
            {
                tracked.job->setStatus(QString());
            }
        }
        else
        {
            auto *job = new TransferJob(id, outbound, device, this);
            connect(job, &TransferJob::cancelRequested, m_core, &CoreBridge::cancel);
            if (const int files = outbound ? m_core->outgoingFileCount(id) : 0; files > 0)
            {
                job->setSummary(i18np("%1 file", "%1 files", files));
            }
            m_tracker.registerJob(job);
            job->start();
            tracked.job = job;
        }
        tracked.job->setProgress(acked, total);

        // Instantaneous speed: bytes since the last sample, at most twice a second, lightly smoothed.
        const qint64 now = m_clock.elapsed();
        if (tracked.sampleMs == 0)
        {
            tracked.sampleMs = now;
            tracked.sampleBytes = acked;
        }
        else if (now - tracked.sampleMs >= 500)
        {
            const double instant = acked >= tracked.sampleBytes ? double(acked - tracked.sampleBytes) * 1000.0 / double(now - tracked.sampleMs) : 0.0;
            tracked.speed = tracked.speed == 0 ? instant : 0.5 * tracked.speed + 0.5 * instant;
            tracked.job->setSpeed(qulonglong(tracked.speed));
            tracked.sampleMs = now;
            tracked.sampleBytes = acked;
        }
    }

    if (!TransferModel::isFinal(state))
    {
        return;
    }
    tracked.finished = true;
    const bool success = state == QLatin1String("Finished");
    // A received file gets its own "Files received" notification: close the job without a second one.
    if (tracked.job)
    {
        // Nothing moved yet: the job only said "Connecting", the failure notification follows.
        tracked.job->finish(success, (success && !outbound && !isText) || !tracked.started);
    }

    const QString who = device.isEmpty() ? i18n("a nearby device") : device;
    const QString refusal = CoreBridge::refusalText(info.value(QStringLiteral("reason")).toString());
    if (!outbound && !refusal.isEmpty())
    {
        // The library refused the offer itself (too many files, not enough space, ...). Always shown:
        // the in-app toast does not give the reason.
        notifyProblem(QStringLiteral("transferFailed"), i18nc("@title:notification", "Offer Refused"), i18n("From %1: %2", who.toHtmlEscaped(), refusal));
    }
    else if (outbound && !tracked.started && (state == QLatin1String("Disconnected") || state == QLatin1String("Rejected")))
    {
        // The library reports a refusal by the other device as "Disconnected"; before any data
        // moved that is the most likely reason (it can also be a full disk or a timeout). Always
        // shown: the in-app toast does not say what to check.
        const QString reason = id.startsWith(QLatin1String("ble:"))
                                   ? i18n("Could not start the transfer to %1 over Bluetooth\nCheck that Bluetooth is on and the device is nearby, then try again", who.toHtmlEscaped())
                                   : i18n("%1 declined the transfer or could not receive it\nCheck the other device and its free space, then try again", who.toHtmlEscaped());
        notifyProblem(QStringLiteral("transferFailed"), i18nc("@title:notification", "Transfer Not Accepted"), reason);
    }
    else if (m_windowActive && m_windowActive())
    {
        // The window is in front: the in-app toast says how the transfer ended (on any page).
    }
    else if (success && !outbound)
    {
        notifyReceived(id, who, info, isText);
    }
    else if (state == QLatin1String("Disconnected"))
    {
        notifyProblem(QStringLiteral("transferFailed"), i18nc("@title:notification", "Transfer Interrupted"), i18n("The connection with %1 was lost\nKeep both devices close and awake, then try again", who.toHtmlEscaped()));
    }
}

void TransferWatcher::notifyConsent(const QString &id, const QString &device, const QString &pin, quint64 token, Tracked &tracked)
{
    auto *notification = new KNotification(QStringLiteral("incomingTransfer"), KNotification::Persistent);
    notification->setIconName(QStringLiteral("io.github.the_beper.KuickShare"));
    notification->setTitle(i18nc("@title:notification", "Incoming Transfer"));
    // It waits for an answer: Normal, as the HIG asks, and persistent so that it is not missed.
    notification->setUrgency(KNotification::NormalUrgency);
    const QString who = device.isEmpty() ? i18n("A nearby device") : device.toHtmlEscaped();
    notification->setText(pin.isEmpty() ? i18n("%1 wants to share with you", who) : i18n("%1 wants to share with you\nPIN: %2", who, pin.toHtmlEscaped()));

    // Every answer carries the token of this prompt: if the core restarted and gave the id to
    // another transfer, it refuses the answer.
    auto *accept = notification->addAction(i18nc("@action:button", "Accept"));
    connect(accept, &KNotificationAction::activated, this, [this, id, token]
            { m_core->answerWithToken(id, token, QStringLiteral("accept")); });
    // Trusting a device is a feature that can be turned off in the settings. It needs the sender's
    // MAC address, which only exists for transfers over the network (id "ip:port"), not for Bluetooth
    // ones (id "ble-N"). The core binds the trust to the address it read when the transfer arrived,
    // and refuses (leaving the transfer waiting) when it cannot.
    if (m_core->trustedDevicesEnabled() && !tracked.noTrust && id.contains(QLatin1Char(':')))
    {
        auto *trust = notification->addAction(i18nc("@action:button", "Accept and Trust"));
        connect(trust, &KNotificationAction::activated, this, [this, id, token]
                { m_core->answerWithToken(id, token, QStringLiteral("accept_and_trust")); });
        notification->setText(notification->text() + QLatin1Char('\n')
                              + i18n("“Accept and Trust” recognises this device by its name and network address, which another device on the same network can copy"));
    }
    auto *decline = notification->addAction(i18nc("@action:button", "Decline"));
    connect(decline, &KNotificationAction::activated, this, [this, id, token]
            { m_core->answerWithToken(id, token, QStringLiteral("decline")); });

    notification->sendEvent();
    tracked.consent = notification;
    tracked.consentToken = token;
}

void TransferWatcher::notifyReceived(const QString &id, const QString &who, const QVariantMap &info, bool isText)
{
    auto *notification = new KNotification(QStringLiteral("transferFinished"));
    notification->setUrgency(KNotification::NormalUrgency);
    notification->setIconName(QStringLiteral("io.github.the_beper.KuickShare"));

    const QString payloadKind = info.value(QStringLiteral("payload_kind")).toString();
    const QString text = info.value(QStringLiteral("text")).toString().isEmpty() ? info.value(QStringLiteral("preview")).toString() : info.value(QStringLiteral("text")).toString();

    if (isText)
    {
        static const QRegularExpression linkPattern(QStringLiteral("^https?://\\S+$"), QRegularExpression::CaseInsensitiveOption);
        const bool isLink = payloadKind == QLatin1String("Url") || linkPattern.match(text.trimmed()).hasMatch();
        notification->setTitle(isLink ? i18nc("@title:notification", "Link Received") : i18nc("@title:notification", "Text Received"));
        notification->setText(i18n("From %1:\n%2", who.toHtmlEscaped(), shortened(text).toHtmlEscaped()));
        auto *copy = notification->addAction(isLink ? i18nc("@action:button", "Copy Link") : i18nc("@action:button", "Copy Text"));
        connect(copy, &KNotificationAction::activated, this, [text]
                { QGuiApplication::clipboard()->setText(text); });
        if (isLink)
        {
            auto *open = notification->addAction(i18nc("@action:button", "Open Link"));
            connect(open, &KNotificationAction::activated, this, [text]
                    { QDesktopServices::openUrl(QUrl(text.trimmed())); });
        }
        auto *view = notification->addAction(i18nc("@action:button", "View"));
        connect(view, &KNotificationAction::activated, this, [this, id]
                { Q_EMIT viewRequested(id); });
    }
    else
    {
        const QString dir = m_core->downloadDir().isEmpty() ? QStandardPaths::writableLocation(QStandardPaths::DownloadLocation) : m_core->downloadDir();
        notification->setTitle(i18nc("@title:notification", "Files Received"));
        notification->setText(i18n("From %1", who.toHtmlEscaped()));
        // Several files: only the folder makes sense.
        const QStringList saved = info.value(QStringLiteral("saved_paths")).toString().split(QLatin1Char('\n'), Qt::SkipEmptyParts);
        if (saved.size() == 1)
        {
            auto *openFile = notification->addAction(i18nc("@action:button", "Open File"));
            connect(openFile, &KNotificationAction::activated, this, [path = saved.first()]
                    { QDesktopServices::openUrl(QUrl::fromLocalFile(path)); });
        }
        auto *open = notification->addAction(i18nc("@action:button", "Open Folder"));
        connect(open, &KNotificationAction::activated, this, [dir]
                { QDesktopServices::openUrl(QUrl::fromLocalFile(dir)); });
    }
    notification->sendEvent();
}

void TransferWatcher::notifyProblem(const QString &eventId, const QString &title, const QString &text)
{
    auto *notification = new KNotification(eventId);
    notification->setUrgency(KNotification::NormalUrgency);
    notification->setIconName(QStringLiteral("dialog-warning"));
    notification->setTitle(title);
    notification->setText(text);
    notification->sendEvent();
}
