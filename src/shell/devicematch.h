#pragma once

#include <QList>
#include <QString>

/// A nearby device as the `--device` option sees it.
struct DeviceRef
{
    QString id;
    QString name;
};

struct DeviceMatch
{
    enum class Kind
    {
        None,
        Unique,
        Ambiguous
    };
    Kind kind = Kind::None;
    QString id; // set for Unique
};

/// The device whose whole name equals `name`, ignoring case and surrounding spaces.
inline DeviceMatch matchDevice(const QList<DeviceRef> &devices, const QString &name)
{
    const QString wanted = name.trimmed();
    DeviceMatch result;
    for (const DeviceRef &device : devices)
    {
        if (device.name.trimmed().compare(wanted, Qt::CaseInsensitive) != 0)
        {
            continue;
        }
        if (result.kind == DeviceMatch::Kind::Unique)
        {
            return {DeviceMatch::Kind::Ambiguous, {}};
        }
        result = {DeviceMatch::Kind::Unique, device.id};
    }
    return result;
}
