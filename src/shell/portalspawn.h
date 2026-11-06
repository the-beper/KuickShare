#pragma once

// The arguments of the Flatpak portal calls that need unusual D-Bus types. Two callers
// (`corebridge.cpp`, `updatewatcher.cpp`) build the same body: no `a{sv}` maps (the portal
// wants `a{uh}` for file descriptors and `a{ss}` for the environment) and the arguments as
// `aay`, each NUL-terminated, because the portal reads argv byte strings.

#include <QByteArray>
#include <QDBusArgument>
#include <QMetaType>
#include <QString>
#include <QStringList>
#include <QVariant>

namespace PortalSpawn
{
    // A plain QVariantMap would be marshalled as a{sv}; the portal wants these maps with other types.
    inline QVariant emptyMap(QMetaType keyType, QMetaType valueType)
    {
        QDBusArgument argument;
        argument.beginMap(keyType, valueType);
        argument.endMap();
        return QVariant::fromValue(argument);
    }

    // `items` as an array of NUL-terminated byte strings (`aay`), the portal's argv shape.
    inline QVariant byteArgv(const QStringList &items)
    {
        // QByteArrayList is not registered for D-Bus marshalling (the smoke test caught that),
        // and a plain binary narration of `aay` is not either: the argument is built by hand.
        QDBusArgument argument;
        argument.beginArray(QMetaType::fromType<QByteArray>());
        for (const QString &item : items)
        {
            argument << item.toUtf8() + '\0';
        }
        argument.endArray();
        return QVariant::fromValue(argument);
    }
}
