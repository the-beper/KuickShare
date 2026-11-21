#pragma once

#include <QDBusError>
#include <QString>

namespace CoreError
{
    enum class Kind
    {
        Unreachable, // nobody answered: no core, or none in time
        NotRunning,  // the core answered that its sharing engine is stopped
        Other
    };

    /// The reply says nobody answered (no core, or none in time), as opposed to the core refusing.
    inline bool unreachable(const QDBusError &error)
    {
        switch (error.type())
        {
        case QDBusError::ServiceUnknown:
        case QDBusError::NoReply:
        case QDBusError::Disconnected:
        case QDBusError::NoServer:
        case QDBusError::TimedOut:
        case QDBusError::UnknownObject:
            return true;
        default:
            return false;
        }
    }

    inline Kind kindOf(const QDBusError &error)
    {
        if (unreachable(error))
        {
            return Kind::Unreachable;
        }
        if (error.name().endsWith(QLatin1String(".Error.NotRunning")))
        {
            return Kind::NotRunning;
        }
        return Kind::Other;
    }
}
