#include "transfermodel.h"

#include <QDateTime>

TransferModel::TransferModel(QObject *parent)
    : QAbstractListModel(parent)
{
    m_speedTimer.setInterval(1000);
    connect(&m_speedTimer, &QTimer::timeout, this, [this] {
        for (int row = 0; row < m_entries.size(); ++row) {
            if (m_entries.at(row).sampleMs >= 0) {
                emit dataChanged(index(row), index(row), {SpeedRole, EtaSecondsRole});
            }
        }
    });
}

int TransferModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_entries.size();
}

QVariant TransferModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid))
    {
        return {};
    }
    const Entry &entry = m_entries.at(index.row());
    switch (role)
    {
    case IdRole:
        return entry.id;
    case KindRole:
        return entry.info.value(QStringLiteral("kind"));
    case StateRole:
        return entry.info.value(QStringLiteral("state"));
    case DeviceRole:
        return entry.info.value(QStringLiteral("device"));
    case PinRole:
        return entry.info.value(QStringLiteral("pin"));
    case PreviewRole:
        return entry.info.value(QStringLiteral("preview"));
    case TotalBytesRole:
        return entry.info.value(QStringLiteral("total_bytes"));
    case AckBytesRole:
        return entry.info.value(QStringLiteral("ack_bytes"));
    case FilesRole:
    {
        const QString files = entry.info.value(QStringLiteral("files")).toString();
        return files.isEmpty() ? QStringList() : files.split(QLatin1Char('\n'));
    }
    case TextRole:
        return entry.info.value(QStringLiteral("text"));
    case PayloadKindRole:
        return entry.info.value(QStringLiteral("payload_kind"));
    case TimeRole:
        return entry.info.value(QStringLiteral("time"));
    case TrustedRole:
        return entry.info.value(QStringLiteral("trusted"));
    case AlwaysAcceptedRole:
        return entry.info.value(QStringLiteral("always_accepted"));
    case HistoryUidRole:
        return entry.info.value(QStringLiteral("history_uid"));
    case SpeedRole:
        return isFinal(entry.info.value(QStringLiteral("state")).toString()) || speedStale(entry) ? 0.0 : entry.speed;
    case EtaSecondsRole:
    {
        // -1 while unknown: finished, no size, or no speed estimate yet.
        const qulonglong total = entry.info.value(QStringLiteral("total_bytes")).toULongLong();
        const qulonglong acked = entry.info.value(QStringLiteral("ack_bytes")).toULongLong();
        if (isFinal(entry.info.value(QStringLiteral("state")).toString()) || total == 0 || entry.speed <= 0 || speedStale(entry)) {
            return -1;
        }
        return acked >= total ? 0 : qlonglong(double(total - acked) / entry.speed + 0.5);
    }
    case SavedPathsRole:
        return entry.info.value(QStringLiteral("saved_paths")).toString().split(QLatin1Char('\n'), Qt::SkipEmptyParts);
    case ConsentRole:
        return entry.info.value(QStringLiteral("consent")).toULongLong();
    case ReasonRole:
        return entry.info.value(QStringLiteral("reason")).toString();
    }
    return {};
}

QHash<int, QByteArray> TransferModel::roleNames() const
{
    return {
        {IdRole, "transferId"},
        {KindRole, "kind"},
        {StateRole, "transferState"},
        {DeviceRole, "device"},
        {PinRole, "pin"},
        {PreviewRole, "preview"},
        {TotalBytesRole, "totalBytes"},
        {AckBytesRole, "ackBytes"},
        {FilesRole, "files"},
        {TextRole, "bodyText"},
        {PayloadKindRole, "payloadKind"},
        {TimeRole, "time"},
        {TrustedRole, "trusted"},
        {AlwaysAcceptedRole, "alwaysAccepted"},
        {HistoryUidRole, "historyUid"},
        {SpeedRole, "speed"},
        {EtaSecondsRole, "etaSeconds"},
        {SavedPathsRole, "savedPaths"},
        {ConsentRole, "consent"},
        {ReasonRole, "reason"},
    };
}

bool TransferModel::isFinal(const QString &state)
{
    return state == QLatin1String("Finished") || state == QLatin1String("Cancelled") || state == QLatin1String("Rejected") || state == QLatin1String("Disconnected");
}

bool TransferModel::hasActive() const
{
    for (const Entry &entry : m_entries) {
        const QString state = entry.info.value(QStringLiteral("state")).toString();
        if (state == QLatin1String("SendingFiles") || state == QLatin1String("ReceivingFiles")) {
            return true;
        }
    }
    return false;
}

bool TransferModel::hasLive() const
{
    for (const Entry &entry : m_entries) {
        if (!isFinal(entry.info.value(QStringLiteral("state")).toString())) {
            return true;
        }
    }
    return false;
}

void TransferModel::updateBusy()
{
    bool busy = false;
    bool waiting = false;
    for (const Entry &entry : m_entries)
    {
        const QString state = entry.info.value(QStringLiteral("state")).toString();
        const bool waitsForUs = entry.info.value(QStringLiteral("kind")).toString() == QLatin1String("inbound") && state == QLatin1String("WaitingForUserConsent");
        waiting = waiting || waitsForUs;
        busy = busy || (!isFinal(state) && !waitsForUs);
    }
    if (busy != m_busy)
    {
        m_busy = busy;
        emit busyChanged();
    }
    if (waiting != m_waiting)
    {
        m_waiting = waiting;
        emit waitingChanged();
    }
}

QVariantMap TransferModel::info(const QString &id) const
{
    for (const Entry &entry : m_entries)
    {
        if (entry.id == id)
        {
            return entry.info;
        }
    }
    return {};
}

void TransferModel::sampleSpeed(Entry &entry, qint64 nowMs)
{
    const QString state = entry.info.value(QStringLiteral("state")).toString();
    const qulonglong acked = entry.info.value(QStringLiteral("ack_bytes")).toULongLong();
    if ((state != QLatin1String("SendingFiles") && state != QLatin1String("ReceivingFiles")) || entry.info.value(QStringLiteral("total_bytes")).toULongLong() == 0) {
        entry.sampleMs = -1;
        entry.speed = 0;
        return;
    }
    // Bytes since the last sample, at most twice a second, lightly smoothed (as in TransferWatcher).
    if (entry.sampleMs < 0) {
        entry.sampleMs = nowMs;
        entry.sampleBytes = acked;
    } else if (nowMs - entry.sampleMs >= 500) {
        const double instant = acked >= entry.sampleBytes ? double(acked - entry.sampleBytes) * 1000.0 / double(nowMs - entry.sampleMs) : 0.0;
        entry.speed = entry.speed == 0 ? instant : 0.5 * entry.speed + 0.5 * instant;
        entry.sampleMs = nowMs;
        entry.sampleBytes = acked;
    }
}

bool TransferModel::speedStale(const Entry &entry) const
{
    return m_now() - entry.lastEventMs > 3000;
}

void TransferModel::refreshSpeedTimer()
{
    bool sampling = false;
    for (const Entry &entry : std::as_const(m_entries)) {
        if (entry.sampleMs >= 0) {
            sampling = true;
            break;
        }
    }
    if (sampling && !m_speedTimer.isActive()) {
        m_speedTimer.start();
    } else if (!sampling) {
        m_speedTimer.stop();
    }
}

void TransferModel::update(const QString &id, const QVariantMap &info)
{
    const qint64 nowMs = m_now();
    for (int row = 0; row < m_entries.size(); ++row)
    {
        if (m_entries[row].id != id)
        {
            continue;
        }
        // Outgoing transfers are keyed by the device address, which repeats: an event that restarts
        // a finished transfer is a new one, and a second final state doesn't replace the first.
        const bool wasFinal = isFinal(m_entries[row].info.value(QStringLiteral("state")).toString());
        if (wasFinal)
        {
            if (isFinal(info.value(QStringLiteral("state")).toString()))
            {
                return;
            }
            break;
        }
        // The core made a new consent for this id: another transfer reuses it (ids like `ble-1`
        // restart at every run), so the old row is over and nothing of it carries into the new one.
        const QString oldState = m_entries[row].info.value(QStringLiteral("state")).toString();
        const quint64 oldConsent = m_entries[row].info.value(QStringLiteral("consent")).toULongLong();
        const quint64 newConsent = info.value(QStringLiteral("consent")).toULongLong();
        const bool newOffer = info.value(QStringLiteral("state")).toString() == QLatin1String("WaitingForUserConsent") && newConsent != 0
                              && info.value(QStringLiteral("kind")).toString() == QLatin1String("inbound");
        if (newOffer && ((oldConsent != 0 && oldConsent != newConsent) || oldState != QLatin1String("WaitingForUserConsent")))
        {
            m_entries[row].info.insert(QStringLiteral("state"), QStringLiteral("Disconnected"));
            emit dataChanged(index(row), index(row));
            updateBusy();
            emit transferEnded(id, QStringLiteral("Disconnected"));
            break;
        }
        // Later events may omit fields (e.g. no metadata yet): keep the last known values.
        for (auto it = info.cbegin(); it != info.cend(); ++it)
        {
            const bool emptyString = it.value().typeId() == QMetaType::QString && it.value().toString().isEmpty();
            if (!emptyString)
            {
                m_entries[row].info.insert(it.key(), it.value());
            }
        }
        // The state is always authoritative, even when empty.
        m_entries[row].info.insert(QStringLiteral("state"), info.value(QStringLiteral("state")));
        m_entries[row].lastEventMs = nowMs;
        sampleSpeed(m_entries[row], nowMs);
        refreshSpeedTimer();
        emit dataChanged(index(row), index(row));
        updateBusy();
        if (isFinal(info.value(QStringLiteral("state")).toString()))
        {
            emit transferEnded(id, info.value(QStringLiteral("state")).toString());
        }
        return;
    }
    QVariantMap first = info;
    first.insert(QStringLiteral("time"), QDateTime::currentSecsSinceEpoch());
    beginInsertRows({}, 0, 0);
    m_entries.prepend({id, first});
    m_entries.first().lastEventMs = nowMs;
    sampleSpeed(m_entries.first(), nowMs);
    refreshSpeedTimer();
    endInsertRows();
    updateBusy();
    if (isFinal(first.value(QStringLiteral("state")).toString()))
    {
        emit transferEnded(id, first.value(QStringLiteral("state")).toString());
    }
}

quint64 TransferModel::beginPending(const QString &id, QVariantMap row)
{
    row.insert(QStringLiteral("kind"), QStringLiteral("outbound"));
    row.insert(QStringLiteral("state"), QStringLiteral("Starting"));
    update(id, row);
    return ++m_pendingNumber[id];
}

bool TransferModel::failPending(const QString &id, quint64 number)
{
    if (m_pendingNumber.value(id) == number && info(id).value(QStringLiteral("state")).toString() == QLatin1String("Starting"))
    {
        update(id, {{QStringLiteral("state"), QStringLiteral("Disconnected")}});
        return true;
    }
    return false;
}

QStringList TransferModel::disconnectLive()
{
    // Only the newest row of an id can be live, so `update` reaches it.
    QStringList ids;
    for (const Entry &entry : std::as_const(m_entries)) {
        if (!isFinal(entry.info.value(QStringLiteral("state")).toString())) {
            ids << entry.id;
        }
    }
    for (const QString &id : std::as_const(ids)) {
        update(id, {{QStringLiteral("state"), QStringLiteral("Disconnected")}});
    }
    return ids;
}

bool TransferModel::cancelPending(const QString &id)
{
    if (info(id).value(QStringLiteral("state")).toString() != QLatin1String("Starting"))
    {
        return false;
    }
    update(id, {{QStringLiteral("state"), QStringLiteral("Cancelled")}});
    return true;
}

void TransferModel::addHistory(const QVariantMap &info)
{
    beginInsertRows({}, m_entries.size(), m_entries.size());
    const qulonglong uid = info.value(QStringLiteral("history_uid")).toULongLong();
    m_entries.append({QStringLiteral("history-%1").arg(uid > 0 ? uid : qulonglong(m_entries.size())), info});
    endInsertRows();
}

void TransferModel::remove(int row)
{
    if (row < 0 || row >= m_entries.size())
    {
        return;
    }
    beginRemoveRows({}, row, row);
    m_entries.removeAt(row);
    endRemoveRows();
    updateBusy();
}

void TransferModel::removeHistory(qulonglong uid)
{
    for (int row = 0; row < m_entries.size(); ++row)
    {
        if (m_entries[row].info.value(QStringLiteral("history_uid")).toULongLong() == uid)
        {
            remove(row);
            return;
        }
    }
}

void TransferModel::clearFinished()
{
    for (int row = m_entries.size() - 1; row >= 0; --row)
    {
        if (isFinal(m_entries[row].info.value(QStringLiteral("state")).toString()))
        {
            beginRemoveRows({}, row, row);
            m_entries.removeAt(row);
            endRemoveRows();
        }
    }
}

void TransferModel::clearAll()
{
    beginResetModel();
    m_entries.clear();
    endResetModel();
    updateBusy();
}
