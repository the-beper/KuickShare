#pragma once

// Pure logic of the Wi-Fi requirements report (no i18n, no D-Bus, QtCore only), used by CoreBridge and tested in
// tests/shell/requirementsreport_test.cpp.
//
// Public API, all in `namespace RequirementsReport`:
//
//   constants      kSchemaVersion (1), kMaxBytes (64 KiB), kMaxRows (64), kMaxDetails (200), kMaxVerification (200),
//                  kMaxDevice (32), kMaxToken (64), kMaxReportLine (300), kDefaultTtlMs (30000)
//   enums          Status {Unknown, Ready, Warning, Blocked, Checking}
//                  Capability {Unknown, Available, Degraded, Unavailable}
//                  Category {Unknown, Critical, Warning, Security, Info}
//                  Error {None, Empty, Json, Schema, Limits}
//   Row            id, status, category, reasonCode, details, device, verification, affects (subset of
//                  "sending", "receiving", "join_network")
//   Parsed         error (default Empty), generatedAtMs, ttlMs, packaging ("flatpak", "appimage", "native" or
//                  "unknown"), coreVersion, sending / receiving / joinNetwork (Capability), rows (QList<Row>);
//                  ok() is true when error == Error::None
//   QString clean(const QString &text, int maxChars)
//                  host text made safe: control, format and line/paragraph separator characters removed, "<" and ">"
//                  replaced by look-alikes (as CoreBridge::plain), trimmed, cut to `maxChars` UTF-16 units
//   Parsed parse(const QByteArray &json)
//                  requires an object with schema_version == 1 and a generated_at_ms >= 0; more than kMaxBytes bytes
//                  or kMaxRows rows give Error::Limits; unknown enum strings become Unknown; every text goes through
//                  `clean`; rows without an id are dropped. Never throws, whatever the input.
//   QString statusCode(Status), capabilityCode(Capability), categoryCode(Category)
//                  the lower-case words of the wire format ("ready", "degraded", "critical", ...; "unknown" for Unknown)
//   qint64 staleInMs(qint64 generatedAtMs, qint64 ttlMs, qint64 nowMs)
//                  milliseconds until the report is out of date, in [0, ttlMs] (a generation time in the future counts
//                  as now)
//   QString formatReport(const Parsed &report, const QString &shellVersion, bool stale)
//                  the text of "Copy diagnostic report": versions, packaging, UTC time, the three capabilities and one
//                  line per row (id, status, reason code, category, verification). `details` and `device` are left out.
//                  Plain text, every line at most kMaxReportLine characters.
//   RequestTracker
//                  Ticket {epoch, seq}
//                  Ticket begin()                       a new request: same epoch, seq one higher than any before
//                  void invalidate()                    the core restarted or went away: next epoch, replies of older
//                                                       epochs are never accepted
//                  bool isCurrent(Ticket) const         the epoch is current and no newer request was begun
//                  bool sameEpoch(Ticket) const         the epoch is current
//                  bool accept(Ticket) const            a reply: true only if isCurrent and no signal was accepted after
//                                                       the request was begun (local order; generated_at_ms, the core's
//                                                       wall clock, is never compared)
//                  bool acceptSignal()                  a WifiRequirementsChanged signal: always true in the current
//                                                       epoch; it supersedes the requests in flight
//                  quint64 epoch() const

#include <QByteArray>
#include <QChar>
#include <QDateTime>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonParseError>
#include <QJsonValue>
#include <QList>
#include <QString>
#include <QStringList>

#include <algorithm>
#include <cmath>

namespace RequirementsReport
{
    constexpr int kSchemaVersion = 1;
    constexpr int kMaxBytes = 64 * 1024;
    constexpr int kMaxRows = 64;
    constexpr int kMaxDetails = 200;
    constexpr int kMaxVerification = 200;
    constexpr int kMaxDevice = 32;
    constexpr int kMaxToken = 64;
    constexpr int kMaxReportLine = 300;
    constexpr qint64 kDefaultTtlMs = 30000;

    enum class Status
    {
        Unknown,
        Ready,
        Warning,
        Blocked,
        Checking
    };

    enum class Capability
    {
        Unknown,
        Available,
        Degraded,
        Unavailable
    };

    enum class Category
    {
        Unknown,
        Critical,
        Warning,
        Security,
        Info
    };

    enum class Error
    {
        None,
        Empty,
        Json,
        Schema,
        Limits
    };

    struct Row
    {
        QString id;
        Status status = Status::Unknown;
        Category category = Category::Unknown;
        QString reasonCode;
        QString details;
        QString device;
        QString verification;
        QStringList affects;
    };

    struct Parsed
    {
        Error error = Error::Empty;
        qint64 generatedAtMs = 0;
        qint64 ttlMs = kDefaultTtlMs;
        QString packaging = QStringLiteral("unknown");
        QString coreVersion;
        Capability sending = Capability::Unknown;
        Capability receiving = Capability::Unknown;
        Capability joinNetwork = Capability::Unknown;
        QList<Row> rows;

        bool ok() const { return error == Error::None; }
    };

    inline QString clean(const QString &text, int maxChars)
    {
        QString out;
        out.reserve(qMin(int(text.size()), maxChars + 1));
        for (const QChar c : text)
        {
            switch (c.category())
            {
            case QChar::Other_Control:
            case QChar::Other_Format:
            case QChar::Separator_Line:
            case QChar::Separator_Paragraph:
                continue;
            default:
                break;
            }
            if (c == QLatin1Char('<'))
            {
                out += QChar(0x2039);
            }
            else if (c == QLatin1Char('>'))
            {
                out += QChar(0x203A);
            }
            else
            {
                out += c;
            }
        }
        out = out.trimmed();
        if (out.size() > maxChars)
        {
            out.truncate(maxChars);
            if (!out.isEmpty() && out.back().isHighSurrogate())
            {
                out.chop(1);
            }
            out = out.trimmed();
        }
        return out;
    }

    namespace detail
    {
        /// An identifier of the wire format: letters, digits, "_", "." and "-" only.
        inline QString token(const QJsonValue &value)
        {
            QString out;
            for (const QChar c : value.toString())
            {
                if (out.size() >= kMaxToken)
                {
                    break;
                }
                if ((c >= QLatin1Char('a') && c <= QLatin1Char('z')) || (c >= QLatin1Char('A') && c <= QLatin1Char('Z')) || (c >= QLatin1Char('0') && c <= QLatin1Char('9')) || c == QLatin1Char('_') || c == QLatin1Char('.') || c == QLatin1Char('-'))
                {
                    out += c;
                }
            }
            return out;
        }

        inline Status status(const QJsonValue &value)
        {
            const QString s = value.toString();
            if (s == QLatin1String("ready"))
                return Status::Ready;
            if (s == QLatin1String("warning"))
                return Status::Warning;
            if (s == QLatin1String("blocked"))
                return Status::Blocked;
            if (s == QLatin1String("checking"))
                return Status::Checking;
            return Status::Unknown;
        }

        inline Capability capability(const QJsonValue &value)
        {
            const QString s = value.toString();
            if (s == QLatin1String("available"))
                return Capability::Available;
            if (s == QLatin1String("degraded") || s == QLatin1String("limited"))
                return Capability::Degraded;
            if (s == QLatin1String("unavailable"))
                return Capability::Unavailable;
            return Capability::Unknown;
        }

        inline Category category(const QJsonValue &value)
        {
            const QString s = value.toString();
            if (s == QLatin1String("critical"))
                return Category::Critical;
            if (s == QLatin1String("warning"))
                return Category::Warning;
            if (s == QLatin1String("security"))
                return Category::Security;
            if (s == QLatin1String("info"))
                return Category::Info;
            return Category::Unknown;
        }

        inline bool number(const QJsonValue &value, qint64 min, qint64 max, qint64 *out)
        {
            if (!value.isDouble())
            {
                return false;
            }
            const double d = value.toDouble();
            // Whole numbers only: 1.5 is not schema version 1 (nor a valid time).
            if (!std::isfinite(d) || d < double(min) || d > double(max) || d != std::floor(d))
            {
                return false;
            }
            *out = static_cast<qint64>(d);
            return true;
        }
    } // namespace detail

    inline Parsed parse(const QByteArray &json)
    {
        Parsed out;
        if (json.isEmpty())
        {
            out.error = Error::Empty;
            return out;
        }
        if (json.size() > kMaxBytes)
        {
            out.error = Error::Limits;
            return out;
        }
        QJsonParseError parseError;
        const QJsonDocument doc = QJsonDocument::fromJson(json, &parseError);
        if (parseError.error != QJsonParseError::NoError || !doc.isObject())
        {
            out.error = Error::Json;
            return out;
        }
        const QJsonObject root = doc.object();
        qint64 schema = 0;
        if (!detail::number(root.value(QLatin1String("schema_version")), 0, 1000000, &schema) || schema != kSchemaVersion)
        {
            out.error = Error::Schema;
            return out;
        }
        qint64 generated = 0;
        if (!detail::number(root.value(QLatin1String("generated_at_ms")), 0, qint64(1) << 52, &generated))
        {
            out.error = Error::Schema;
            return out;
        }
        const QJsonValue rows = root.value(QLatin1String("requirements"));
        if (!rows.isArray())
        {
            out.error = Error::Schema;
            return out;
        }
        const QJsonArray rowArray = rows.toArray();
        if (rowArray.size() > kMaxRows)
        {
            out.error = Error::Limits;
            return out;
        }

        out.generatedAtMs = generated;
        qint64 ttl = kDefaultTtlMs;
        if (detail::number(root.value(QLatin1String("ttl_ms")), 1000, 3600000, &ttl))
        {
            out.ttlMs = ttl;
        }
        const QString packaging = root.value(QLatin1String("packaging")).toString();
        if (packaging == QLatin1String("flatpak") || packaging == QLatin1String("appimage") || packaging == QLatin1String("native"))
        {
            out.packaging = packaging;
        }
        out.coreVersion = clean(root.value(QLatin1String("core_version")).toString(), kMaxDevice);
        const QJsonObject caps = root.value(QLatin1String("capabilities")).toObject();
        out.sending = detail::capability(caps.value(QLatin1String("sending")));
        out.receiving = detail::capability(caps.value(QLatin1String("receiving")));
        out.joinNetwork = detail::capability(caps.value(QLatin1String("join_network")));

        for (const QJsonValue &value : rowArray)
        {
            if (!value.isObject())
            {
                continue;
            }
            const QJsonObject o = value.toObject();
            Row row;
            row.id = detail::token(o.value(QLatin1String("id")));
            if (row.id.isEmpty())
            {
                continue;
            }
            row.status = detail::status(o.value(QLatin1String("status")));
            row.category = detail::category(o.value(QLatin1String("category")));
            row.reasonCode = detail::token(o.value(QLatin1String("reason_code")));
            row.details = clean(o.value(QLatin1String("details")).toString(), kMaxDetails);
            row.device = clean(o.value(QLatin1String("device")).toString(), kMaxDevice);
            row.verification = clean(o.value(QLatin1String("verification")).toString(), kMaxVerification);
            for (const QJsonValue &a : o.value(QLatin1String("affects")).toArray())
            {
                const QString s = a.toString();
                if ((s == QLatin1String("sending") || s == QLatin1String("receiving") || s == QLatin1String("join_network")) && !row.affects.contains(s))
                {
                    row.affects << s;
                }
            }
            out.rows.append(row);
        }
        out.error = Error::None;
        return out;
    }

    inline QString statusCode(Status status)
    {
        switch (status)
        {
        case Status::Ready:
            return QStringLiteral("ready");
        case Status::Warning:
            return QStringLiteral("warning");
        case Status::Blocked:
            return QStringLiteral("blocked");
        case Status::Checking:
            return QStringLiteral("checking");
        case Status::Unknown:
            break;
        }
        return QStringLiteral("unknown");
    }

    inline QString capabilityCode(Capability capability)
    {
        switch (capability)
        {
        case Capability::Available:
            return QStringLiteral("available");
        case Capability::Degraded:
            return QStringLiteral("degraded");
        case Capability::Unavailable:
            return QStringLiteral("unavailable");
        case Capability::Unknown:
            break;
        }
        return QStringLiteral("unknown");
    }

    inline QString categoryCode(Category category)
    {
        switch (category)
        {
        case Category::Critical:
            return QStringLiteral("critical");
        case Category::Warning:
            return QStringLiteral("warning");
        case Category::Security:
            return QStringLiteral("security");
        case Category::Info:
            return QStringLiteral("info");
        case Category::Unknown:
            break;
        }
        return QStringLiteral("unknown");
    }

    inline qint64 staleInMs(qint64 generatedAtMs, qint64 ttlMs, qint64 nowMs)
    {
        const qint64 age = qMax<qint64>(0, nowMs - generatedAtMs);
        return qBound<qint64>(0, ttlMs - age, qMax<qint64>(0, ttlMs));
    }

    inline QString formatReport(const Parsed &report, const QString &shellVersion, bool stale)
    {
        auto line = [](const QString &text)
        { return clean(text, kMaxReportLine); };
        QStringList lines;
        lines << QStringLiteral("KuickShare diagnostic report");
        if (!report.ok())
        {
            lines << line(QStringLiteral("Shell %1, schema %2").arg(clean(shellVersion, kMaxDevice)).arg(kSchemaVersion));
            lines << QStringLiteral("No report available");
            return lines.join(QLatin1Char('\n')) + QLatin1Char('\n');
        }
        lines << line(QStringLiteral("Shell %1, core %2, schema %3, packaging %4")
                          .arg(clean(shellVersion, kMaxDevice), report.coreVersion.isEmpty() ? QStringLiteral("unknown") : report.coreVersion)
                          .arg(kSchemaVersion)
                          .arg(report.packaging));
        lines << line(QStringLiteral("Checked %1 (stale: %2)")
                          .arg(QDateTime::fromMSecsSinceEpoch(report.generatedAtMs).toUTC().toString(Qt::ISODate), stale ? QStringLiteral("yes") : QStringLiteral("no")));
        lines << line(QStringLiteral("Sending %1 | Receiving %2 | Join network %3")
                          .arg(capabilityCode(report.sending), capabilityCode(report.receiving), capabilityCode(report.joinNetwork)));
        for (const Row &row : report.rows)
        {
            QString text = QStringLiteral("%1 %2 %3 [%4]")
                               .arg(row.id, statusCode(row.status), row.reasonCode.isEmpty() ? QStringLiteral("-") : row.reasonCode, categoryCode(row.category));
            if (!row.verification.isEmpty())
            {
                text += QLatin1Char(' ') + row.verification;
            }
            lines << line(text);
        }
        return lines.join(QLatin1Char('\n')) + QLatin1Char('\n');
    }

    /// Decides which replies of the core may change what the user sees.
    class RequestTracker
    {
    public:
        struct Ticket
        {
            quint64 epoch = 0;
            quint64 seq = 0;
        };

        Ticket begin()
        {
            Ticket ticket;
            ticket.epoch = m_epoch;
            ticket.seq = ++m_seq;
            return ticket;
        }

        void invalidate()
        {
            ++m_epoch;
        }

        bool sameEpoch(Ticket ticket) const { return ticket.epoch == m_epoch; }
        bool isCurrent(Ticket ticket) const { return ticket.epoch == m_epoch && ticket.seq == m_seq; }

        /// A reply is applied if it answers the newest request and no signal came in since that request was sent.
        /// The order is the local one (the order messages arrive in), never the core's wall clock, which can go back.
        bool accept(Ticket ticket) const { return isCurrent(ticket) && ticket.seq > m_signalSeq; }

        /// A signal always applies (D-Bus keeps the order of the core's messages) and supersedes the requests in flight.
        bool acceptSignal()
        {
            m_signalSeq = m_seq;
            return true;
        }

        quint64 epoch() const { return m_epoch; }

    private:
        quint64 m_epoch = 0;
        quint64 m_seq = 0;
        quint64 m_signalSeq = 0;
    };
} // namespace RequirementsReport
