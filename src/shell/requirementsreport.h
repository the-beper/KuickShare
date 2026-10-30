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
//   Component      key, status, informational, rows
//   QString componentOf(const QString &id)
//                  the part of the system a row belongs to: "bluetooth" (bluez.*), "networkmanager" (nm.service,
//                  nm.networking, nm.permissions), "wifi" (the other nm.* rows, wifi.client, wifi.ap), "dnsmasq"
//                  (wifi.dhcp), "firewall" (firewall.*), "flatpak" (sandbox.*); an id this version does not know is its
//                  own key
//   QList<Component> components(const QList<Row> &rows)
//                  one entry per component, in the order the rows first name them. `rows` keeps those that are not
//                  Ready and the Ready ones with a reason other than "ok" (a note such as "binary_present"), most
//                  severe first (Blocked, Warning, Unknown, Checking, any Info row, then the Ready notes): a row that
//                  only points at another one ("skipped", "prerequisite_blocked", "prerequisite_uncertain") is dropped
//                  when a row that is not Ready has a reason of its own, and only the first is kept otherwise.
//                  `status` is the first row's (Ready when none is left); `informational` is true when that row is an
//                  Info row that is not Ready, which no capability depends on
//   QStringList overrideOptions(const QString &details)
//                  the `flatpak override` options for the words of a sandbox.permissions "missing" row ("missing:
//                  network bluetooth bluez networkmanager firewalld polkit"); unknown words are left out
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
#include <utility>

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

    struct Component
    {
        QString key;
        Status status = Status::Ready;
        bool informational = false;
        QList<Row> rows;
    };

    inline QString componentOf(const QString &id)
    {
        if (id.startsWith(QLatin1String("bluez.")))
            return QStringLiteral("bluetooth");
        if (id == QLatin1String("nm.service") || id == QLatin1String("nm.networking") || id == QLatin1String("nm.permissions"))
            return QStringLiteral("networkmanager");
        if (id.startsWith(QLatin1String("nm.")) || id == QLatin1String("wifi.client") || id == QLatin1String("wifi.ap"))
            return QStringLiteral("wifi");
        if (id == QLatin1String("wifi.dhcp"))
            return QStringLiteral("dnsmasq");
        if (id.startsWith(QLatin1String("firewall.")))
            return QStringLiteral("firewall");
        if (id.startsWith(QLatin1String("sandbox.")))
            return QStringLiteral("flatpak");
        return id;
    }

    namespace detail
    {
        /// How much a row needs attention. An Info row that is not Ready only says what was not looked at.
        inline int severity(const Row &row)
        {
            if (row.status == Status::Ready)
                return 0;
            if (row.category == Category::Info)
                return 1;
            switch (row.status)
            {
            case Status::Checking:
                return 2;
            case Status::Warning:
                return 4;
            case Status::Blocked:
                return 5;
            case Status::Unknown:
            case Status::Ready:
                break;
            }
            return 3;
        }

        /// A reason that only points at another row, which says what is wrong.
        inline bool derived(const Row &row)
        {
            return row.reasonCode == QLatin1String("skipped") || row.reasonCode == QLatin1String("prerequisite_blocked") || row.reasonCode == QLatin1String("prerequisite_uncertain");
        }
    } // namespace detail

    inline QList<Component> components(const QList<Row> &rows)
    {
        QList<Component> out;
        for (const Row &row : rows)
        {
            const QString key = componentOf(row.id);
            auto it = std::find_if(out.begin(), out.end(), [&key](const Component &c) { return c.key == key; });
            if (it == out.end())
            {
                Component component;
                component.key = key;
                out.append(component);
                it = out.end() - 1;
            }
            // A Ready row with a reason of its own carries a note ("dnsmasq is installed").
            if (row.status != Status::Ready || (!row.reasonCode.isEmpty() && row.reasonCode != QLatin1String("ok")))
            {
                it->rows.append(row);
            }
        }
        for (Component &c : out)
        {
            std::stable_sort(c.rows.begin(), c.rows.end(), [](const Row &a, const Row &b) { return detail::severity(a) > detail::severity(b); });
            const bool explained = std::any_of(c.rows.cbegin(), c.rows.cend(), [](const Row &r) { return r.status != Status::Ready && !detail::derived(r); });
            bool keptOne = false;
            c.rows.removeIf([&](const Row &r) {
                if (!detail::derived(r))
                    return false;
                if (explained || keptOne)
                    return true;
                keptOne = true; // the most severe, since the rows are sorted
                return false;
            });
            if (!c.rows.isEmpty())
            {
                c.status = c.rows.first().status;
                c.informational = c.status != Status::Ready && c.rows.first().category == Category::Info;
            }
        }
        return out;
    }

    inline QStringList overrideOptions(const QString &details)
    {
        static const std::pair<const char *, const char *> options[] = {
            {"network", "--share=network"},
            {"bluetooth", "--allow=bluetooth"},
            {"bluez", "--system-talk-name=org.bluez"},
            {"networkmanager", "--system-talk-name=org.freedesktop.NetworkManager"},
            {"firewalld", "--system-talk-name=org.fedoraproject.FirewallD1"},
            {"polkit", "--system-talk-name=org.freedesktop.PolicyKit1"},
        };
        const QLatin1String prefix("missing:");
        const QString words = details.startsWith(prefix) ? details.mid(prefix.size()) : details;
        QStringList out;
        for (const QString &word : words.split(QLatin1Char(' '), Qt::SkipEmptyParts))
        {
            for (const auto &[name, option] : options)
            {
                const QString text = QLatin1String(option);
                if (word == QLatin1String(name) && !out.contains(text))
                {
                    out << text;
                }
            }
        }
        return out;
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
