#include "transfermodel.h"

#include <QDateTime>

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
    for (const Entry &entry : m_entries)
    {
        const QString state = entry.info.value(QStringLiteral("state")).toString();
        const bool waitsForUs = entry.info.value(QStringLiteral("kind")).toString() == QLatin1String("inbound") && state == QLatin1String("WaitingForUserConsent");
        if (!isFinal(state) && !waitsForUs)
        {
            busy = true;
            break;
        }
    }
    if (busy != m_busy)
    {
        m_busy = busy;
        emit busyChanged();
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

void TransferModel::update(const QString &id, const QVariantMap &info)
{
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
