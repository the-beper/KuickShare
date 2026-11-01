#include "transferjob.h"

#include <utility>

#include <KLocalizedString>

TransferJob::TransferJob(const QString &id, bool outbound, const QString &device, QObject *parent)
    : KJob(parent), m_id(id), m_device(device), m_outbound(outbound)
{
    setCapabilities(KJob::Killable);
}

void TransferJob::start()
{
    emitDescription();
}

void TransferJob::setStatus(const QString &status, bool cancellable)
{
    const bool killable = status.isEmpty() || cancellable;
    if (m_status == status && (capabilities() == KJob::Killable) == killable)
    {
        return;
    }
    m_status = status;
    setCapabilities(killable ? KJob::Killable : KJob::NoCapabilities);
    emitDescription();
}

void TransferJob::emitDescription()
{
    Q_EMIT description(this,
                       !m_status.isEmpty() ? m_status : m_outbound ? i18nc("@title:notification", "Sending Files")
                                                                  : i18nc("@title:notification", "Receiving Files"),
                       qMakePair(m_outbound ? i18nc("@label transfer destination", "To") : i18nc("@label transfer source", "From"), m_device),
                       m_summary.isEmpty() ? QPair<QString, QString>() : qMakePair(i18nc("@label", "Files"), m_summary));
}

void TransferJob::setProgress(qulonglong processed, qulonglong total)
{
    if (m_done)
    {
        return;
    }
    setTotalAmount(KJob::Bytes, total);
    setProcessedAmount(KJob::Bytes, processed);
}

void TransferJob::setSpeed(qulonglong bytesPerSecond)
{
    if (!m_done)
    {
        emitSpeed(bytesPerSecond);
    }
}

void TransferJob::finish(bool success, bool quietly)
{
    if (m_done)
    {
        return;
    }
    m_done = true;
    if (!success)
    {
        setError(KJob::UserDefinedError);
        setErrorText(i18n("The transfer was interrupted; check the connection and try again"));
    }
    if (quietly)
    {
        kill(KJob::Quietly);
    }
    else
    {
        emitResult();
    }
}

bool TransferJob::doKill()
{
    // Closing a finished job quietly must not cancel the transfer again.
    if (!std::exchange(m_done, true))
    {
        Q_EMIT cancelRequested(m_id);
    }
    return true;
}
