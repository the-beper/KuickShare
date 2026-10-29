#include "corebridge.h"

#include <QCoreApplication>
#include <QDBusConnection>
#include <QDBusConnectionInterface>
#include <QDBusError>
#include <QDBusPendingCall>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>
#include <QDBusVariant>
#include <QDebug>
#include <QClipboard>
#include <QDesktopServices>
#include <QFileInfo>
#include <QDir>
#include <QDateTime>
#include <QMimeData>
#include <QImage>
#include <QGuiApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QStandardPaths>
#include <QLocale>
#include <QMimeDatabase>
#include <QRegularExpression>

#include <KLocalizedString>

#include "autostart.h"
#include "folderexpand.h"
#include "openablefile.h"
#include <kcoreaddons.h>
#include "dolphinmenu.h"
#include "integration.h"
#include "launcher.h"

namespace
{
    const QString service = QStringLiteral("io.github.the_beper.KuickShare");
    const QString path = QStringLiteral("/io/github/the_beper/KuickShare");
    const QString iface = QStringLiteral("io.github.the_beper.KuickShare1");
    const QString propsIface = QStringLiteral("org.freedesktop.DBus.Properties");

    QDBusMessage methodCall(const QString &interface, const QString &method)
    {
        return QDBusMessage::createMethodCall(service, path, interface, method);
    }
}

namespace
{
    /// The reply says nobody answered (no core, or none in time), as opposed to the core refusing.
    bool coreUnreachable(const QDBusError &error)
    {
        switch (error.type()) {
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
}

namespace
{
    // Images pasted from the clipboard are saved here to be sent; they are of no use after that.
    void removeOldClipboardImages()
    {
        const QDir dir(QStandardPaths::writableLocation(QStandardPaths::CacheLocation) + QStringLiteral("/clipboard"));
        const QDateTime limit = QDateTime::currentDateTime().addSecs(-3600);
        for (const QFileInfo &file : dir.entryInfoList(QDir::Files)) {
            if (file.lastModified() < limit) {
                QFile::remove(file.absoluteFilePath());
            }
        }
    }
}

CoreBridge::CoreBridge(QObject *parent)
    : QObject(parent), m_autostart(Autostart::isEnabled())
{
    connect(&m_transfers, &TransferModel::busyChanged, this, &CoreBridge::transferBusyChanged);
    connect(&m_transfers, &TransferModel::transferEnded, this, &CoreBridge::transferEnded);
    auto bus = QDBusConnection::sessionBus();
    bus.connect(service, path, iface, QStringLiteral("TransferUpdated"), this, SLOT(onTransferUpdated(QString, QVariantMap)));
    bus.connect(service, path, iface, QStringLiteral("DeviceFound"), this, SLOT(onDeviceFound(QString, QVariantMap)));
    bus.connect(service, path, iface, QStringLiteral("DeviceLost"), this, SLOT(onDeviceLost(QString)));
    bus.connect(service, path, iface, QStringLiteral("PairingCode"), this, SLOT(onPairingCode(QString, QString)));
    bus.connect(service, path, iface, QStringLiteral("PairingFinished"), this, SLOT(onPairingFinished(bool, QString, QString)));
    bus.connect(service, path, iface, QStringLiteral("WifiUpgradeBlocked"), this, SLOT(onWifiUpgradeBlocked(QString)));
    bus.connect(service, path, iface, QStringLiteral("WifiRequirementsChanged"), this, SLOT(onWifiRequirementsSignal(QString)));
    bus.connect(service, path, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("PropertiesChanged"), this, SLOT(onPropertiesChanged(QString, QVariantMap, QStringList)));
    bus.connect(service, path, iface, QStringLiteral("TrustedChanged"), this, SLOT(loadTrustedDevices()));

    auto *watcher = new QDBusServiceWatcher(service, bus, QDBusServiceWatcher::WatchForOwnerChange, this);
    connect(watcher, &QDBusServiceWatcher::serviceRegistered, this, &CoreBridge::onServiceRegistered);
    connect(watcher, &QDBusServiceWatcher::serviceUnregistered, this, &CoreBridge::onServiceUnregistered);

    // Stopping discovery leaks an mDNS daemon in the library, and restarting it is slow: keep it
    // running for a while after the user leaves the Send page.
    m_discoveryStopTimer.setSingleShot(true);
    m_discoveryStopTimer.setInterval(30000);
    connect(&m_discoveryStopTimer, &QTimer::timeout, this, &CoreBridge::stopDiscoveryNow);

    // A report goes stale when its time to live is over (see `adoptRequirements`).
    m_requirementsStaleTimer.setSingleShot(true);
    connect(&m_requirementsStaleTimer, &QTimer::timeout, this, [this]
            {
        if (m_requirementsHasReport && !m_requirementsStale) {
            markRequirementsStale(m_requirementsError);
        } });
    rebuildRequirementsMap();

    removeOldClipboardImages();

    // What the Paste buttons can do depends on what is on the clipboard.
    connect(QGuiApplication::clipboard(), &QClipboard::dataChanged, this, &CoreBridge::updateClipboard);
    updateClipboard();

    if (bus.interface()->isServiceRegistered(service))
    {
        onServiceRegistered();
    }
    else
    {
        startCore();
    }
}

CoreBridge::~CoreBridge()
{
    if (m_core.state() != QProcess::NotRunning)
    {
        m_core.terminate();
        m_core.waitForFinished(3000);
    }
}

void CoreBridge::startCore()
{
    m_core.setProcessChannelMode(QProcess::ForwardedChannels);
    m_core.start(QCoreApplication::applicationDirPath() + QStringLiteral("/kuickshared"));
    m_serviceStarting = true;
    emit serviceStartingChanged();
    // Starting ends when the service shows up on the bus (see onServiceRegistered), or fails here.
    auto stopStarting = [this] {
        if (m_serviceStarting && !m_connected) {
            m_serviceStarting = false;
            emit serviceStartingChanged();
        }
    };
    disconnect(&m_core, nullptr, this, nullptr);
    connect(&m_core, &QProcess::errorOccurred, this, stopStarting);
    connect(&m_core, &QProcess::finished, this, stopStarting);
}

void CoreBridge::startService()
{
    if (m_core.state() != QProcess::NotRunning) {
        // A service that is up but not on the bus is stuck: start over.
        m_core.kill();
        m_core.waitForFinished(2000);
    }
    startCore();
}

void CoreBridge::setUpdateInstalled(bool installed)
{
    if (m_updateInstalled != installed) {
        m_updateInstalled = installed;
        emit updateInstalledChanged();
    }
}

bool CoreBridge::transferActive() const
{
    return m_transfers.hasActive();
}

bool CoreBridge::transferLive() const
{
    return m_transfers.hasLive();
}

void CoreBridge::resetSettings()
{
    callAsync(methodCall(iface, QStringLiteral("ResetSettings")), [this] {
        m_devices.clear(); // sharing restarted: the old cards are gone
        // What the app keeps itself: no autostart, and the Dolphin entry on, as in a fresh install.
        Autostart::setEnabled(false);
        m_autostart = Autostart::isEnabled();
        emit autostartChanged();
        DolphinMenu::setEnabled(true);
        refreshProperties();
    });
}

void CoreBridge::updateClipboard()
{
    const QMimeData *data = QGuiApplication::clipboard()->mimeData();
    bool hasText = false;
    bool hasFiles = false;
    if (data) {
        hasText = data->hasText() && !data->text().isEmpty();
        if (data->hasUrls()) {
            for (const QUrl &url : data->urls()) {
                const QFileInfo info(url.toLocalFile());
                if (url.isLocalFile() && (info.isFile() || info.isDir())) {
                    hasFiles = true;
                    break;
                }
            }
        }
        // An image that comes without text (a screenshot, a picture copied from a viewer)
        hasFiles = hasFiles || (data->hasImage() && !hasText);
    }
    if (hasText != m_clipboardHasText || hasFiles != m_clipboardHasFiles) {
        m_clipboardHasText = hasText;
        m_clipboardHasFiles = hasFiles;
        emit clipboardChanged();
    }
}

QString CoreBridge::plain(const QString &text)
{
    QString out = text;
    out.replace(QLatin1Char('<'), QChar(0x2039)).replace(QLatin1Char('>'), QChar(0x203A));
    return out;
}

QString CoreBridge::qtVersion() const
{
    return QString::fromLatin1(qVersion());
}

QString CoreBridge::frameworksVersion() const
{
    return KCoreAddons::versionString();
}

QStringList CoreBridge::clipboardFiles() const
{
    const QMimeData *data = QGuiApplication::clipboard()->mimeData();
    QStringList files;
    if (!data) {
        return files;
    }
    if (data->hasUrls()) {
        for (const QUrl &url : data->urls()) {
            const QString path = url.toLocalFile();
            const QFileInfo info(path);
            if (url.isLocalFile() && (info.isFile() || info.isDir())) {
                files << path;
            }
        }
        if (!files.isEmpty()) {
            return files;
        }
    }
    if (data->hasImage() && !data->hasText()) {
        const QImage image = qvariant_cast<QImage>(data->imageData());
        const QString dir = QStandardPaths::writableLocation(QStandardPaths::CacheLocation) + QStringLiteral("/clipboard");
        if (!image.isNull() && QDir().mkpath(dir)) {
            const QString path = dir + QStringLiteral("/clipboard-") + QDateTime::currentDateTime().toString(QStringLiteral("yyyyMMdd-HHmmss-zzz")) + QStringLiteral(".png");
            if (image.save(path, "PNG")) {
                files << path;
            }
        }
    }
    return files;
}

void CoreBridge::onServiceRegistered()
{
    m_connected = true;
    if (m_serviceStarting) {
        m_serviceStarting = false;
        emit serviceStartingChanged();
    }
    m_suspended = false; // a fresh core is not suspended
    emit suspendedChanged();
    emit connectedChanged();
    // A report of an earlier core says nothing about this one, and its late replies are ignored.
    m_requirementsTracker.invalidate();
    setRequirementsBusy(false);
    markRequirementsStale(QString());
    refreshProperties();
    m_transfers.clearAll();
    emit transfersReset();
    loadHistory();
    loadHistorySize();
    loadTrustedDevices();
    loadAllowedDevices();
    if (m_discoveryResume)
    {
        m_discoveryResume = false;
        startDiscovery();
    }
}

void CoreBridge::onServiceUnregistered()
{
    m_connected = false;
    if (m_visibleUntil != 0) {
        m_visibleUntil = 0; // temporary visibility does not outlive a core
        emit visibleUntilChanged();
    }
    m_discoveryResume = m_discoveryRunning;
    m_discoveryRunning = false;
    m_devices.clear();
    m_requirementsTracker.invalidate();
    setRequirementsBusy(false);
    markRequirementsStale(QStringLiteral("unavailable"));
    // The core will not report these any more.
    for (const QString &id : m_transfers.disconnectLive())
    {
        emit transferChanged(id);
    }
    emit connectedChanged();
}

void CoreBridge::callAsync(const QDBusMessage &message, const std::function<void()> &onSuccess, const std::function<void()> &onError)
{
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(message), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, onSuccess, onError](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<> reply = *w;
        if (reply.isError()) {
            emit errorOccurred(reply.error().message());
            refreshProperties(); // drop any optimistic value the UI is showing
            if (onError) {
                onError();
            }
        } else if (onSuccess) {
            onSuccess();
        } });
}

void CoreBridge::call(const QString &method, const QVariantList &args)
{
    auto msg = methodCall(iface, method);
    msg.setArguments(args);
    callAsync(msg);
}

void CoreBridge::refreshProperties()
{
    auto msg = methodCall(propsIface, QStringLiteral("GetAll"));
    msg << iface;
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(msg), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<QVariantMap> reply = *w;
        if (reply.isError()) {
            qWarning() << "GetAll failed:" << reply.error().message();
            return;
        }
        const QVariantMap props = reply.value();
        m_visible = props.value(QStringLiteral("Visible"), m_visible).toBool();
        const qint64 visibleUntil = props.value(QStringLiteral("VisibleUntil"), 0).toLongLong();
        const bool visibleUntilDiffers = visibleUntil != m_visibleUntil;
        m_visibleUntil = visibleUntil;
        m_deviceName = props.value(QStringLiteral("DeviceName")).toString();
        m_downloadDir = props.value(QStringLiteral("DownloadDir")).toString();
        m_staticPort = props.value(QStringLiteral("StaticPort")).toInt();
        const bool suspended = props.value(QStringLiteral("Suspended"), m_suspended).toBool();
        const bool suspendedDiffers = suspended != m_suspended;
        m_suspended = suspended;
        const bool hide = props.value(QStringLiteral("HideWhenDnd"), m_hideWhenDnd).toBool();
        const bool hideChanged = hide != m_hideWhenDnd;
        m_hideWhenDnd = hide;
        const bool historyEnabled = props.value(QStringLiteral("HistoryEnabled"), m_historyEnabled).toBool();
        const int keepDays = props.value(QStringLiteral("HistoryKeepDays"), m_historyKeepDays).toInt();
        const bool historyChanged = historyEnabled != m_historyEnabled;
        const bool keepChanged = keepDays != m_historyKeepDays;
        m_historyEnabled = historyEnabled;
        m_historyKeepDays = keepDays;
        const bool tray = props.value(QStringLiteral("TrayIcon"), m_trayIcon).toBool();
        const bool dolphin = props.value(QStringLiteral("DolphinIntegration"), m_dolphinIntegration).toBool();
        const bool trusted = props.value(QStringLiteral("TrustedDevicesEnabled"), m_trustedDevicesEnabled).toBool();
        const bool trustedChanged = trusted != m_trustedDevicesEnabled;
        m_trustedDevicesEnabled = trusted;
        const bool reset = props.value(QStringLiteral("TrustedDevicesReset"), m_trustedDevicesReset).toBool();
        const bool resetChanged = reset != m_trustedDevicesReset;
        m_trustedDevicesReset = reset;
        const bool wifiUpgrade = props.value(QStringLiteral("WifiUpgrade"), m_wifiUpgrade).toBool();
        const bool wifiChanged = wifiUpgrade != m_wifiUpgrade;
        m_wifiUpgrade = wifiUpgrade;
        const bool insecure = props.value(QStringLiteral("InsecureHotspotAllowed"), m_insecureHotspotAllowed).toBool();
        const bool insecureChanged = insecure != m_insecureHotspotAllowed;
        m_insecureHotspotAllowed = insecure;
        const bool trayChanged = tray != m_trayIcon;
        const bool dolphinChanged = dolphin != m_dolphinIntegration;
        m_trayIcon = tray;
        m_dolphinIntegration = dolphin;
        // Turned off in the settings (e.g. before an update put the entry back): make the menu match.
        if (!dolphin && DolphinMenu::exists())
        {
            DolphinMenu::setEnabled(false);
        }
        // A Flatpak has no installer to write the entry, so the app writes it.
        if (dolphin && Launcher::inFlatpak() && !DolphinMenu::exists() && !DolphinMenu::setEnabled(true))
        {
            qWarning() << "Could not write the Dolphin entry";
        }
        emit visibleChanged();
        if (visibleUntilDiffers) {
            emit visibleUntilChanged();
        }
        emit deviceNameChanged();
        emit downloadDirChanged();
        emit staticPortChanged();
        if (suspendedDiffers) {
            emit suspendedChanged();
        }
        if (hideChanged) {
            emit hideWhenDndChanged();
        }
        if (historyChanged) {
            emit historyEnabledChanged();
        }
        if (trayChanged) {
            emit trayIconChanged();
        }
        if (trustedChanged) {
            emit trustedDevicesEnabledChanged();
        }
        if (resetChanged) {
            emit trustedDevicesResetChanged();
        }
        if (dolphinChanged) {
            emit dolphinIntegrationChanged();
        }
        if (wifiChanged) {
            emit wifiUpgradeChanged();
        }
        if (insecureChanged) {
            emit insecureHotspotAllowedChanged();
        }
        if (keepChanged) {
            emit historyKeepDaysChanged();
        } });
}

void CoreBridge::setRemoteProperty(const QString &name, const QVariant &value)
{
    auto msg = methodCall(propsIface, QStringLiteral("Set"));
    msg << iface << name << QVariant::fromValue(QDBusVariant(value));
    // Name and port restart sharing, which forgets the devices found so far.
    const bool restarts = name == QLatin1String("DeviceName") || name == QLatin1String("StaticPort");
    callAsync(msg, [this, restarts]
              {
                  if (restarts)
                  {
                      m_devices.clear();
                  }
                  refreshProperties();
              });
}

void CoreBridge::loadHistory()
{
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(methodCall(iface, QStringLiteral("History"))), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            qWarning() << "History failed:" << reply.error().message();
            return;
        }
        const QJsonArray entries = QJsonDocument::fromJson(reply.value().toUtf8()).array();
        for (const QJsonValue &value : entries) {
            const QJsonObject object = value.toObject();
            QStringList files;
            for (const QJsonValue &file : object.value(QStringLiteral("files")).toArray()) {
                files << file.toString();
            }
            const qlonglong bytes = object.value(QStringLiteral("total_bytes")).toVariant().toLongLong();
            const QString state = object.value(QStringLiteral("state")).toString();
            m_transfers.addHistory({
                {QStringLiteral("history_uid"), object.value(QStringLiteral("uid")).toVariant().toULongLong()},
                {QStringLiteral("trusted"), object.value(QStringLiteral("trusted")).toBool() ? plain(object.value(QStringLiteral("device")).toString()) : QString()},
                {QStringLiteral("kind"), object.value(QStringLiteral("kind")).toString()},
                {QStringLiteral("state"), state},
                {QStringLiteral("device"), plain(object.value(QStringLiteral("device")).toString())},
                {QStringLiteral("payload_kind"), object.value(QStringLiteral("payload_kind")).toString()},
                {QStringLiteral("files"), plain(files.join(QLatin1Char('\n')))},
                {QStringLiteral("text"), object.value(QStringLiteral("text")).toString()},
                {QStringLiteral("total_bytes"), bytes},
                {QStringLiteral("ack_bytes"), state == QLatin1String("Finished") ? bytes : 0},
                {QStringLiteral("time"), object.value(QStringLiteral("time")).toVariant().toLongLong()},
            });
        } });
}

void CoreBridge::loadTrustedDevices()
{
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(methodCall(iface, QStringLiteral("TrustedDevices"))), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            return;
        }
        m_trustedDevices.clear();
        for (const QJsonValue &value : QJsonDocument::fromJson(reply.value().toUtf8()).array()) {
            QVariantMap device = value.toObject().toVariantMap();
            device[QStringLiteral("name")] = plain(device.value(QStringLiteral("name")).toString());
            m_trustedDevices << device;
        }
        emit trustedDevicesChanged(); });
}

void CoreBridge::loadAllowedDevices()
{
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(methodCall(iface, QStringLiteral("AllowedDevices"))), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            return;
        }
        m_allowedDevices.clear();
        for (const QJsonValue &value : QJsonDocument::fromJson(reply.value().toUtf8()).array()) {
            // "name" is what the core matches on (to forget it); "title" is what is shown.
            QVariantMap device = value.toObject().toVariantMap();
            device[QStringLiteral("title")] = plain(device.value(QStringLiteral("name")).toString());
            m_allowedDevices << device;
        }
        emit allowedDevicesChanged(); });
}

void CoreBridge::answerTransfer(const QString &id, const QString &answer)
{
    answerWithToken(id, m_transfers.info(id).value(QStringLiteral("consent")).toULongLong(), answer);
}

void CoreBridge::answerWithToken(const QString &id, quint64 token, const QString &answer)
{
    if (token == 0)
    {
        // The core reported no consent for this transfer: nothing waits for an answer.
        emit answerRefused(id);
        return;
    }
    auto msg = methodCall(iface, QStringLiteral("AnswerTransfer"));
    msg << id << token << answer;
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(msg), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, id](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<> reply = *w;
        if (!reply.isError()) {
            return;
        }
        // The core names why it refused (`<interface>.Error.<Name>`).
        const QString name = reply.error().name();
        const auto is = [&name](const char *suffix) { return name.endsWith(QLatin1String(suffix)); };
        if (is(".Error.NotWaiting") || is(".Error.Expired") || is(".Error.StaleToken")) {
            // Nothing waits for this answer any more: the prompt is stale.
            emit errorOccurred(i18n("This transfer is no longer waiting for an answer"));
            emit answerRefused(id);
            return;
        }
        // The transfer still waits (the sender cannot be trusted, or something else went wrong):
        // the prompt stays, so the user can accept it without trusting.
        if (is(".Error.NoMac")) {
            emit errorOccurred(i18n("This device cannot be trusted: its network address is not known"));
        } else if (is(".Error.NoName")) {
            emit errorOccurred(i18n("This device cannot be trusted: it sent no name"));
        } else if (is(".Error.DeviceChanged")) {
            emit errorOccurred(i18n("This device cannot be trusted: its network address now belongs to another device"));
        } else {
            emit errorOccurred(reply.error().message());
        }
        emit answerNotApplied(id); });
}

QString CoreBridge::refusalText(const QString &reason)
{
    if (reason == QLatin1String("too_many_files"))
    {
        return i18n("The offer has too many files");
    }
    if (reason == QLatin1String("not_enough_space"))
    {
        return i18n("There is not enough free space in the download folder");
    }
    if (reason == QLatin1String("destination"))
    {
        return i18n("The download folder cannot be used");
    }
    if (reason == QLatin1String("bad_name"))
    {
        return i18n("A file name in the offer cannot be saved");
    }
    if (reason == QLatin1String("bad_size") || reason == QLatin1String("duplicate_id"))
    {
        return i18n("The offer is malformed");
    }
    // A code this version does not know.
    return reason.isEmpty() ? QString() : i18n("The offer was refused");
}

namespace
{
    namespace RR = RequirementsReport;

    struct RequirementText
    {
        QString text;
        QString hint;
        QString command;
    };

    QString overrideCommand(const QString &option)
    {
        return QStringLiteral("flatpak override --user %1 %2").arg(option, Launcher::flatpakId());
    }

    QString talkCommand(const QString &busName)
    {
        return overrideCommand(QStringLiteral("--system-talk-name=") + busName);
    }

    /// An interface name that is safe to put in a command (the core sends only such names).
    bool isInterfaceName(const QString &name)
    {
        static const QRegularExpression pattern(QStringLiteral("^[A-Za-z0-9_.:-]{1,32}$"));
        return pattern.match(name).hasMatch();
    }

    QString requirementTitle(const QString &id)
    {
        if (id == QLatin1String("bluez.service")) {
            return i18n("Bluetooth service (BlueZ)");
        }
        if (id == QLatin1String("bluez.adapter")) {
            return i18n("Bluetooth adapter");
        }
        if (id == QLatin1String("bluez.powered")) {
            return i18n("Bluetooth power");
        }
        if (id == QLatin1String("bluez.le")) {
            return i18n("Bluetooth Low Energy");
        }
        if (id == QLatin1String("bluez.advertising")) {
            return i18n("Bluetooth advertising");
        }
        if (id == QLatin1String("bluez.scanning")) {
            return i18n("Bluetooth scanning");
        }
        if (id == QLatin1String("bluez.transport")) {
            return i18n("Bluetooth GATT services");
        }
        if (id == QLatin1String("bluez.l2cap")) {
            return i18n("Bluetooth fast channel (L2CAP)");
        }
        if (id == QLatin1String("nm.service")) {
            return i18n("NetworkManager");
        }
        if (id == QLatin1String("nm.networking")) {
            return i18n("Networking switch");
        }
        if (id == QLatin1String("nm.wifi_enabled")) {
            return i18n("Wi-Fi switch");
        }
        if (id == QLatin1String("nm.hardware_enabled")) {
            return i18n("Wi-Fi radio switch");
        }
        if (id == QLatin1String("nm.device")) {
            return i18n("Wi-Fi adapter");
        }
        if (id == QLatin1String("nm.managed")) {
            return i18n("Wi-Fi adapter management");
        }
        if (id == QLatin1String("nm.firmware")) {
            return i18n("Wi-Fi adapter firmware");
        }
        if (id == QLatin1String("nm.driver")) {
            return i18n("Wi-Fi adapter driver");
        }
        if (id == QLatin1String("nm.permissions")) {
            return i18n("NetworkManager permissions");
        }
        if (id == QLatin1String("wifi.client")) {
            return i18n("Wi-Fi client mode");
        }
        if (id == QLatin1String("wifi.ap")) {
            return i18n("Wi-Fi hotspot mode");
        }
        if (id == QLatin1String("wifi.dhcp")) {
            return i18n("Hotspot addresses (dnsmasq)");
        }
        if (id == QLatin1String("firewall.integration")) {
            return i18n("Firewall (firewalld)");
        }
        if (id == QLatin1String("sandbox.permissions")) {
            return i18n("Flatpak permissions");
        }
        // A requirement this version does not know: its id is all there is to show.
        return id;
    }

    /// What a reason code means and what to do about it. Hints name the one thing to fix (a switched-off adapter is
    /// not a missing package); commands are only shown to be copied, never run.
    RequirementText requirementText(const QString &id, const QString &reason, const QString &device)
    {
        const auto isId = [&id](const char *want) { return id == QLatin1String(want); };
        const auto isReason = [&reason](const char *want) { return reason == QLatin1String(want); };

        // Reasons any requirement can have.
        if (isReason("ok")) {
            return {i18n("Nothing is missing"), {}, {}};
        }
        if (isReason("timeout")) {
            return {i18n("The check took too long"), i18n("Refresh to check again; if it keeps happening, the service it asks may be busy or stuck"), {}};
        }
        if (isReason("bus_unreachable")) {
            return {i18n("The system message bus could not be reached"), i18n("The check needs the system D-Bus; refresh to try again"), {}};
        }
        if (isReason("malformed_reply")) {
            return {i18n("The service gave an answer that could not be understood"), i18n("Refresh to try again; if it persists, update the service"), {}};
        }
        if (isReason("query_failed")) {
            return {i18n("The check failed"), i18n("Refresh to try again"), {}};
        }
        if (isReason("skipped")) {
            return {i18n("Not checked, because something it depends on is not ready"), i18n("Fix the items that are blocked or unknown first"), {}};
        }

        if (isId("bluez.service")) {
            if (isReason("not_running")) {
                return {i18n("The Bluetooth service is not running"), i18n("Start the Bluetooth service; if BlueZ is not installed, install it first"), QStringLiteral("sudo systemctl enable --now bluetooth.service")};
            }
            if (isReason("not_running_activatable")) {
                return {i18n("The Bluetooth service is installed but not running"), i18n("It starts when something needs it; to start it now, use the command"), QStringLiteral("sudo systemctl start bluetooth.service")};
            }
            if (isReason("sandbox_denied")) {
                return {i18n("The sandbox hides the Bluetooth service"), i18n("Allow the sandbox to talk to BlueZ, then restart KuickShare"), talkCommand(QStringLiteral("org.bluez"))};
            }
        } else if (isId("bluez.adapter")) {
            if (isReason("no_adapter")) {
                return {i18n("No Bluetooth adapter was found"), i18n("Plug in or enable a Bluetooth adapter; if one is installed, check that its driver is loaded"), {}};
            }
        } else if (isId("bluez.powered")) {
            if (isReason("powered_off")) {
                return {i18n("The Bluetooth adapter is turned off"), i18n("KuickShare turns it on when it needs it; you can also turn Bluetooth on in the system settings"), {}};
            }
            if (isReason("rfkill_blocked")) {
                return {i18n("Blocked by a radio switch"), i18n("Turn Bluetooth on in the system settings"), {}};
            }
        } else if (isId("bluez.le")) {
            if (isReason("no_le_roles")) {
                return {i18n("The Bluetooth adapter does not support Bluetooth Low Energy"), i18n("Use an adapter that supports Bluetooth 4.0 or newer"), {}};
            }
            if (isReason("roles_unreported")) {
                return {i18n("BlueZ does not report what the adapter can do"), i18n("This may still work; a newer BlueZ reports it"), {}};
            }
        } else if (isId("bluez.advertising")) {
            if (isReason("unsupported")) {
                return {i18n("The Bluetooth adapter cannot advertise"), i18n("Receiving by Bluetooth needs an adapter that can advertise; sending is not affected"), {}};
            }
            if (isReason("no_free_slot")) {
                return {i18n("The adapter has no free advertising slot now"), i18n("Another program uses them; close it and refresh"), {}};
            }
        } else if (isId("bluez.scanning")) {
            if (isReason("no_central_role")) {
                return {i18n("The Bluetooth adapter cannot scan for devices"), i18n("Sending needs an adapter that can scan; use another one"), {}};
            }
            if (isReason("filters_unreported")) {
                return {i18n("BlueZ does not report how the adapter can scan"), i18n("This may still work; a newer BlueZ reports it"), {}};
            }
        } else if (isId("bluez.transport")) {
            if (isReason("no_gatt_manager")) {
                return {i18n("BlueZ offers no GATT services"), i18n("Update BlueZ to a version with GATT support"), {}};
            }
        } else if (isId("bluez.l2cap")) {
            if (isReason("sandbox_denied")) {
                return {i18n("The sandbox does not allow Bluetooth sockets"), i18n("Transfers still work over a slower path; allow Bluetooth for the Flatpak to use the faster one"), overrideCommand(QStringLiteral("--allow=bluetooth"))};
            }
            if (isReason("kernel_unreported")) {
                return {i18n("The kernel does not report Bluetooth socket support"), i18n("Transfers still work over a slower path if it is missing"), {}};
            }
        } else if (isId("nm.service")) {
            if (isReason("not_running")) {
                return {i18n("NetworkManager is not running"), i18n("Wi-Fi Direct needs NetworkManager; start it, or install it if this computer uses another network tool"), QStringLiteral("sudo systemctl enable --now NetworkManager.service")};
            }
            if (isReason("not_running_activatable")) {
                return {i18n("NetworkManager is installed but not running"), i18n("It starts when something needs it; to start it now, use the command"), QStringLiteral("sudo systemctl start NetworkManager.service")};
            }
            if (isReason("sandbox_denied")) {
                return {i18n("The sandbox hides NetworkManager"), i18n("Allow the sandbox to talk to NetworkManager, then restart KuickShare"), talkCommand(QStringLiteral("org.freedesktop.NetworkManager"))};
            }
        } else if (isId("nm.networking")) {
            if (isReason("disabled")) {
                return {i18n("Networking is turned off in NetworkManager"), i18n("Turn networking on in the network settings"), QStringLiteral("nmcli networking on")};
            }
        } else if (isId("nm.wifi_enabled")) {
            if (isReason("disabled")) {
                return {i18n("Wi-Fi is turned off"), i18n("Turn Wi-Fi on in the system settings"), QStringLiteral("nmcli radio wifi on")};
            }
        } else if (isId("nm.hardware_enabled")) {
            if (isReason("switch_off")) {
                return {i18n("A hardware switch has turned the Wi-Fi radio off"), i18n("Use the Wi-Fi switch or key of this computer"), {}};
            }
        } else if (isId("nm.device")) {
            if (isReason("ok_first_of_many")) {
                return {i18n("There are several Wi-Fi adapters; the first one is used"), i18n("If transfers fail, turn off the adapters you do not need"), {}};
            }
            if (isReason("no_wifi_device")) {
                return {i18n("No Wi-Fi adapter was found"), i18n("Plug in a Wi-Fi adapter, or check that the driver of the built-in one is loaded"), {}};
            }
        } else if (isId("nm.managed")) {
            if (isReason("unmanaged")) {
                return {i18n("NetworkManager does not manage the Wi-Fi adapter"), i18n("Let NetworkManager manage it: another network tool or an unmanaged-devices setting may be holding it"), isInterfaceName(device) ? QStringLiteral("nmcli device set %1 managed yes").arg(device) : QString()};
            }
        } else if (isId("nm.firmware")) {
            if (isReason("firmware_missing")) {
                return {i18n("The Wi-Fi adapter needs firmware that is not installed"), i18n("Install the firmware package for the adapter (often linux-firmware), then restart the computer"), {}};
            }
        } else if (isId("nm.driver")) {
            if (isReason("plugin_missing")) {
                return {i18n("The NetworkManager plugin for the Wi-Fi adapter is missing"), i18n("Install the NetworkManager Wi-Fi plugin package of your distribution, then restart NetworkManager"), {}};
            }
            if (isReason("unavailable")) {
                return {i18n("NetworkManager reports the Wi-Fi adapter as unavailable"), i18n("Check that its driver loaded and its radio is on"), {}};
            }
        } else if (isId("nm.permissions")) {
            if (isReason("auth_required")) {
                return {i18n("NetworkManager asks for authentication to control the network"), i18n("The hotspot may prompt for a password or be refused; allow network control for your user in the PolicyKit settings"), {}};
            }
            if (isReason("denied")) {
                return {i18n("NetworkManager does not allow this user to control the network"), i18n("Ask the administrator to allow network control and changes to system connections for your user"), {}};
            }
        } else if (isId("wifi.client")) {
            if (isReason("prerequisite_blocked")) {
                return {i18n("Joining Wi-Fi cannot work: something it needs is blocked"), i18n("Fix the blocked items of NetworkManager and the Wi-Fi adapter"), {}};
            }
            if (isReason("prerequisite_uncertain")) {
                return {i18n("It is not known whether joining Wi-Fi works: something it needs could not be checked"), i18n("Refresh when the items above are sorted out"), {}};
            }
            if (isReason("sandbox_no_network")) {
                return {i18n("The sandbox has no network access"), i18n("Allow the network for the Flatpak"), overrideCommand(QStringLiteral("--share=network"))};
            }
        } else if (isId("wifi.ap")) {
            if (isReason("no_ap_mode")) {
                return {i18n("The Wi-Fi adapter cannot create a hotspot"), i18n("Receiving by Wi-Fi needs an adapter with access point mode; Bluetooth still works"), {}};
            }
            if (isReason("no_wpa2")) {
                return {i18n("The Wi-Fi adapter cannot protect a hotspot with WPA2"), i18n("Receiving by Wi-Fi needs WPA2 on the adapter; Bluetooth still works"), {}};
            }
            if (isReason("no_band")) {
                return {i18n("The Wi-Fi adapter reports no 2.4 GHz or 5 GHz band for a hotspot"), i18n("Receiving by Wi-Fi needs one of them; Bluetooth still works"), {}};
            }
            if (isReason("caps_unreported")) {
                return {i18n("The Wi-Fi driver does not report what the adapter can do"), i18n("The hotspot may still work; it could not be checked"), {}};
            }
            if (isReason("share_auth_required")) {
                return {i18n("NetworkManager asks for authentication to share a connection"), i18n("The hotspot may prompt for a password or be refused; allow it in the PolicyKit settings"), {}};
            }
            if (isReason("share_denied")) {
                return {i18n("This user may not share a connection"), i18n("Ask the administrator to allow sharing connections in NetworkManager"), {}};
            }
            if (isReason("sandbox_no_network")) {
                return {i18n("The sandbox has no network access"), i18n("Allow the network for the Flatpak"), overrideCommand(QStringLiteral("--share=network"))};
            }
        } else if (isId("wifi.dhcp")) {
            if (isReason("binary_present")) {
                return {i18n("dnsmasq is installed"), {}, {}};
            }
            if (isReason("worked_this_session")) {
                return {i18n("The hotspot already handed out addresses in this session"), {}, {}};
            }
            if (isReason("not_found")) {
                return {i18n("dnsmasq was not found"), i18n("The hotspot hands out addresses with dnsmasq; install the dnsmasq or dnsmasq-base package"), {}};
            }
            if (isReason("sandbox_unobservable")) {
                return {i18n("dnsmasq cannot be looked for from inside the sandbox"), i18n("If the hotspot gets no addresses, install dnsmasq or dnsmasq-base on this computer"), {}};
            }
            if (isReason("shared_start_failed")) {
                return {i18n("The hotspot could not start its address service"), i18n("Install the dnsmasq or dnsmasq-base package"), {}};
            }
        } else if (isId("firewall.integration")) {
            if (isReason("not_running")) {
                return {i18n("The firewall service (firewalld) is not running"), i18n("KuickShare cannot open the hotspot port by itself; it asks you before the hotspot uses an open zone"), {}};
            }
            if (isReason("sandbox_denied")) {
                return {i18n("The sandbox hides the firewall service"), i18n("Allow the sandbox to talk to firewalld, then restart KuickShare"), talkCommand(QStringLiteral("org.fedoraproject.FirewallD1"))};
            }
            if (isReason("zone_missing")) {
                return {i18n("The firewall has no zone for the hotspot (nm-shared)"), i18n("The hotspot may be blocked or open; add the zone or check its rules"), {}};
            }
            if (isReason("permission_auth")) {
                return {i18n("Changing the firewall asks for authentication"), i18n("Opening the hotspot port may prompt for a password or be refused"), {}};
            }
            if (isReason("permission_denied")) {
                return {i18n("This user may not change the firewall"), i18n("The hotspot port cannot be opened; ask the administrator to allow it"), {}};
            }
            if (isReason("permission_unverifiable")) {
                return {i18n("It cannot be checked whether this user may change the firewall"), i18n("A sandbox cannot ask for this; the hotspot is tried anyway"), {}};
            }
            if (isReason("denied_this_session")) {
                return {i18n("The firewall refused the hotspot in this session"), i18n("Allow the hotspot in the firewall, or let it use an open zone when asked"), {}};
            }
            if (isReason("consent_given")) {
                return {i18n("You allowed the hotspot to use an open firewall zone in this session"), i18n("Turning the setting off takes this back"), {}};
            }
        } else if (isId("sandbox.permissions")) {
            if (isReason("missing")) {
                return {i18n("The Flatpak lacks permissions it needs"), i18n("Allow the permissions named in the details with flatpak override"), {}};
            }
            if (isReason("filesystem_host")) {
                return {i18n("The Flatpak can see the whole file system"), i18n("It does not need to; take that permission away"), overrideCommand(QStringLiteral("--nofilesystem=host"))};
            }
        }
        // A code this version does not know.
        return {i18n("No more information is available"), {}, {}};
    }

    QString statusText(RR::Status status)
    {
        switch (status) {
        case RR::Status::Ready:
            return i18n("Ready");
        case RR::Status::Warning:
            return i18n("Warning");
        case RR::Status::Blocked:
            return i18n("Blocked");
        case RR::Status::Checking:
            return i18n("Checking");
        case RR::Status::Unknown:
            break;
        }
        return i18n("Unknown");
    }

    QString statusIcon(RR::Status status)
    {
        switch (status) {
        case RR::Status::Ready:
            return QStringLiteral("dialog-ok");
        case RR::Status::Warning:
            return QStringLiteral("dialog-warning");
        case RR::Status::Blocked:
            return QStringLiteral("dialog-error");
        case RR::Status::Checking:
            return QStringLiteral("view-refresh");
        case RR::Status::Unknown:
            break;
        }
        return QStringLiteral("dialog-question");
    }

    QString capabilityLabel(RR::Capability capability)
    {
        switch (capability) {
        case RR::Capability::Available:
            return i18n("Available");
        case RR::Capability::Degraded:
            return i18n("Limited");
        case RR::Capability::Unavailable:
            return i18n("Unavailable");
        case RR::Capability::Unknown:
            break;
        }
        return i18n("Unknown");
    }

    QString capabilityIcon(RR::Capability capability)
    {
        switch (capability) {
        case RR::Capability::Available:
            return QStringLiteral("dialog-ok");
        case RR::Capability::Degraded:
            return QStringLiteral("dialog-warning");
        case RR::Capability::Unavailable:
            return QStringLiteral("dialog-error");
        case RR::Capability::Unknown:
            break;
        }
        return QStringLiteral("dialog-question");
    }

    QVariantMap capabilityMap(RR::Capability capability)
    {
        return {{QStringLiteral("status"), RR::capabilityCode(capability)},
                {QStringLiteral("label"), capabilityLabel(capability)},
                {QStringLiteral("iconName"), capabilityIcon(capability)}};
    }

    QString affectsText(const QStringList &affects)
    {
        QStringList words;
        if (affects.contains(QLatin1String("sending"))) {
            words << i18n("sending");
        }
        if (affects.contains(QLatin1String("receiving"))) {
            words << i18n("receiving");
        }
        if (affects.contains(QLatin1String("join_network"))) {
            words << i18n("joining networks");
        }
        return QLocale().createSeparatedList(words);
    }

    QVariantMap rowMap(const RR::Row &row)
    {
        const RequirementText text = requirementText(row.id, row.reasonCode, row.device);
        return {{QStringLiteral("id"), row.id},
                {QStringLiteral("title"), requirementTitle(row.id)},
                {QStringLiteral("status"), RR::statusCode(row.status)},
                {QStringLiteral("statusText"), statusText(row.status)},
                {QStringLiteral("iconName"), statusIcon(row.status)},
                {QStringLiteral("category"), RR::categoryCode(row.category)},
                {QStringLiteral("reasonCode"), row.reasonCode},
                {QStringLiteral("reasonText"), text.text},
                {QStringLiteral("hint"), text.hint},
                {QStringLiteral("command"), text.command},
                {QStringLiteral("details"), row.details},
                {QStringLiteral("device"), row.device},
                {QStringLiteral("verification"), row.verification},
                {QStringLiteral("affectsText"), affectsText(row.affects)}};
    }

    /// `rows` that feed `capability` and are not ready, or every row when `capability` is empty.
    QVariantMap sectionMap(const QString &key, const QString &title, const QList<RR::Row> &rows, const QString &capability)
    {
        QVariantList list;
        for (const RR::Row &row : rows) {
            if (capability.isEmpty() || (row.status != RR::Status::Ready && row.affects.contains(capability))) {
                list << rowMap(row);
            }
        }
        return {{QStringLiteral("key"), key}, {QStringLiteral("title"), title}, {QStringLiteral("rows"), list}};
    }
}

void CoreBridge::requestWifiRequirements(bool force)
{
    if (!m_connected) {
        markRequirementsStale(QStringLiteral("unavailable"));
        return;
    }
    if (m_wifiRequirementsBusy && !force) {
        return; // the check on its way answers this one too
    }
    // Only the reply of the newest request, from the core that was there when it was sent, is used.
    const RR::RequestTracker::Ticket ticket = m_requirementsTracker.begin();
    setRequirementsBusy(true);
    auto msg = methodCall(iface, QStringLiteral("WifiRequirements"));
    msg << force;
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(msg), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, ticket](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        if (!m_requirementsTracker.sameEpoch(ticket)) {
            return; // the core restarted since: this reply is from before
        }
        const bool latest = m_requirementsTracker.isCurrent(ticket);
        if (latest) {
            setRequirementsBusy(false);
        }
        const QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            qWarning() << "WifiRequirements failed:" << reply.error().name();
            if (latest) {
                markRequirementsStale(coreUnreachable(reply.error()) ? QStringLiteral("unavailable") : QStringLiteral("schema"));
            }
            return;
        }
        const RR::Parsed report = RR::parse(reply.value().toUtf8());
        if (!report.ok()) {
            qWarning() << "WifiRequirements: the reply could not be read";
            if (latest) {
                markRequirementsStale(QStringLiteral("schema"));
            }
            return;
        }
        if (m_requirementsTracker.accept(ticket)) {
            adoptRequirements(report);
        } });
}

void CoreBridge::onWifiRequirementsSignal(const QString &json)
{
    const RequirementsReport::Parsed report = RequirementsReport::parse(json.toUtf8());
    if (!report.ok()) {
        qWarning() << "WifiRequirementsChanged: the report could not be read";
        markRequirementsStale(QStringLiteral("schema"));
        return;
    }
    if (m_requirementsTracker.acceptSignal()) {
        adoptRequirements(report);
    }
}

void CoreBridge::adoptRequirements(const RequirementsReport::Parsed &report)
{
    m_requirementsReport = report;
    m_requirementsHasReport = true;
    m_requirementsError.clear();
    const qint64 left = RR::staleInMs(report.generatedAtMs, report.ttlMs, QDateTime::currentMSecsSinceEpoch());
    m_requirementsStale = left <= 0;
    if (m_requirementsStale) {
        m_requirementsStaleTimer.stop();
    } else {
        m_requirementsStaleTimer.start(static_cast<int>(left));
    }
    rebuildRequirementsMap();
}

void CoreBridge::markRequirementsStale(const QString &error)
{
    m_requirementsStaleTimer.stop();
    m_requirementsStale = true;
    m_requirementsError = error;
    rebuildRequirementsMap();
}

void CoreBridge::setRequirementsBusy(bool busy)
{
    if (busy != m_wifiRequirementsBusy) {
        m_wifiRequirementsBusy = busy;
        emit wifiRequirementsBusyChanged();
    }
}

void CoreBridge::rebuildRequirementsMap()
{
    const RR::Parsed &report = m_requirementsReport;
    const bool valid = m_requirementsHasReport;
    QVariantMap map;
    map.insert(QStringLiteral("valid"), valid);
    map.insert(QStringLiteral("stale"), valid && m_requirementsStale);
    map.insert(QStringLiteral("error"), m_requirementsError);
    map.insert(QStringLiteral("generatedAtMs"), valid ? report.generatedAtMs : qint64(0));
    map.insert(QStringLiteral("ttlMs"), valid ? report.ttlMs : qint64(0));
    map.insert(QStringLiteral("packaging"), valid ? report.packaging : QString());
    map.insert(QStringLiteral("coreVersion"), valid ? report.coreVersion : QString());
    QVariantMap capabilities;
    capabilities.insert(QStringLiteral("sending"), capabilityMap(valid ? report.sending : RR::Capability::Unknown));
    capabilities.insert(QStringLiteral("receiving"), capabilityMap(valid ? report.receiving : RR::Capability::Unknown));
    capabilities.insert(QStringLiteral("joinNetwork"), capabilityMap(valid ? report.joinNetwork : RR::Capability::Unknown));
    map.insert(QStringLiteral("capabilities"), capabilities);
    QVariantList sections;
    if (valid) {
        sections << sectionMap(QStringLiteral("sending"), i18n("Sending"), report.rows, QStringLiteral("sending"))
                 << sectionMap(QStringLiteral("receiving"), i18n("Receiving"), report.rows, QStringLiteral("receiving"))
                 << sectionMap(QStringLiteral("join_network"), i18n("Join network"), report.rows, QStringLiteral("join_network"))
                 << sectionMap(QStringLiteral("system"), i18n("System components"), report.rows, QString());
    }
    map.insert(QStringLiteral("sections"), sections);
    if (map != m_wifiRequirements) {
        m_wifiRequirements = map;
        emit wifiRequirementsChanged();
    }
}

void CoreBridge::copyWifiRequirementsReport()
{
    copyToClipboard(RR::formatReport(m_requirementsHasReport ? m_requirementsReport : RR::Parsed(), QStringLiteral(KUICKSHARE_VERSION), m_requirementsStale));
}

void CoreBridge::acknowledgeTrustedDevicesReset()
{
    if (!m_trustedDevicesReset)
    {
        return;
    }
    m_trustedDevicesReset = false;
    emit trustedDevicesResetChanged();
    // A failure reads the property again, which brings the notice back.
    callAsync(methodCall(iface, QStringLiteral("AcknowledgeTrustedDevicesReset")));
}

void CoreBridge::allowDevice(const QString &transferId)
{
    auto msg = methodCall(iface, QStringLiteral("AllowDevice"));
    msg.setArguments({transferId});
    callAsync(msg, [this]
              { loadAllowedDevices(); });
}

void CoreBridge::forgetAllowed(const QString &name, const QString &mac)
{
    auto msg = methodCall(iface, QStringLiteral("ForgetAllowed"));
    msg.setArguments({name, mac});
    callAsync(msg, [this]
              { loadAllowedDevices(); });
}

void CoreBridge::setHistoryEnabled(bool enabled)
{
    if (enabled == m_historyEnabled)
    {
        return;
    }
    m_historyEnabled = enabled;
    emit historyEnabledChanged();
    setRemoteProperty(QStringLiteral("HistoryEnabled"), enabled);
}

void CoreBridge::setTrayIcon(bool show)
{
    if (show == m_trayIcon) {
        return;
    }
    m_trayIcon = show;
    emit trayIconChanged();
    setRemoteProperty(QStringLiteral("TrayIcon"), show);
}

void CoreBridge::setTrustedDevicesEnabled(bool enabled)
{
    if (enabled == m_trustedDevicesEnabled) {
        return;
    }
    m_trustedDevicesEnabled = enabled;
    emit trustedDevicesEnabledChanged();
    setRemoteProperty(QStringLiteral("TrustedDevicesEnabled"), enabled);
}

void CoreBridge::setWifiUpgrade(bool enabled)
{
    // Not optimistic: `m_wifiUpgrade` only takes what the core reports, so a refusal leaves the setting as it was.
    requestWifiUpgrade(enabled, false);
}

void CoreBridge::requestWifiUpgrade(bool enabled, bool allowInsecure)
{
    auto msg = methodCall(iface, QStringLiteral("RequestWifiUpgrade"));
    msg << enabled << allowInsecure;
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(msg), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<> reply = *w;
        if (!reply.isError()) {
            refreshProperties();
            loadWifiRequirements();
            return;
        }
        const QDBusError error = reply.error();
        QString reason = QStringLiteral("failed");
        if (error.name().endsWith(QLatin1String(".Error.RequirementsBlocked"))) {
            reason = QStringLiteral("requirements_blocked");
        } else if (coreUnreachable(error)) {
            reason = QStringLiteral("service_unavailable");
        }
        if (reason == QLatin1String("failed")) {
            emit errorOccurred(error.message());
        }
        // The switch shows the value of the core again, whatever the user did to it.
        emit wifiUpgradeChanged();
        emit wifiUpgradeRefused(reason);
        if (reason != QLatin1String("service_unavailable")) {
            refreshProperties();
        }
        if (reason == QLatin1String("requirements_blocked")) {
            loadWifiRequirements(); // the dialog the user opens next shows what blocked it
        } });
}

void CoreBridge::enableWifiUpgrade()
{
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(methodCall(iface, QStringLiteral("FirewallManaged"))), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<bool> reply = *w;
        // A core that cannot answer is not a reason to refuse the setting.
        if (reply.isError() || reply.value()) {
            requestWifiUpgrade(true, false);
        } else {
            emit firewallWarning();
        } });
}

void CoreBridge::setDolphinIntegration(bool enabled)
{
    if (enabled == m_dolphinIntegration) {
        return;
    }
    if (!DolphinMenu::setEnabled(enabled)) {
        emit errorOccurred(i18n("Could not update Dolphin's context menu"));
        return;
    }
    m_dolphinIntegration = enabled;
    emit dolphinIntegrationChanged();
    setRemoteProperty(QStringLiteral("DolphinIntegration"), enabled);
}

bool CoreBridge::appImage() const
{
    return Launcher::inAppImage();
}

bool CoreBridge::menuEntry() const
{
    return Integration::menuEntryExists();
}

void CoreBridge::setMenuEntry(bool enabled)
{
    if (enabled == menuEntry()) {
        return;
    }
    if (!(enabled ? Integration::installMenuEntry() : Integration::removeMenuEntry())) {
        emit errorOccurred(i18n("Could not update the application menu"));
    } else {
        Integration::markAsked();
    }
    emit menuEntryChanged();
}

void CoreBridge::integrateAppImage()
{
    if (!Integration::installMenuEntry() || !DolphinMenu::setEnabled(true)) {
        // Not remembered: it is asked again at the next start.
        emit errorOccurred(i18n("Could not add KuickShare to the menus"));
        return;
    }
    Integration::markAsked();
    emit menuEntryChanged();
    // The Dolphin entry now exists whatever the setting was, so the setting must say on.
    m_dolphinIntegration = true;
    emit dolphinIntegrationChanged();
    setRemoteProperty(QStringLiteral("DolphinIntegration"), true);
}

void CoreBridge::declineIntegration()
{
    Integration::markAsked();
    setDolphinIntegration(false);
}

void CoreBridge::setHistoryKeepDays(int days)
{
    if (days == m_historyKeepDays)
    {
        return;
    }
    m_historyKeepDays = days;
    emit historyKeepDaysChanged();
    setRemoteProperty(QStringLiteral("HistoryKeepDays"), QVariant::fromValue(quint32(days)));
}

void CoreBridge::removeTransfer(int row)
{
    if (row < 0 || row >= m_transfers.rowCount())
    {
        return;
    }
    const qulonglong uid = m_transfers.data(m_transfers.index(row), TransferModel::HistoryUidRole).toULongLong();
    if (uid == 0)
    {
        m_transfers.remove(row); // a live row: nothing saved yet
        return;
    }
    auto msg = methodCall(iface, QStringLiteral("RemoveHistory"));
    msg << QVariant::fromValue(quint64(uid));
    callAsync(msg, [this, uid] { m_transfers.removeHistory(uid); });
}

void CoreBridge::clearHistory()
{
    callAsync(methodCall(iface, QStringLiteral("ClearHistory")), [this] {
        m_transfers.clearFinished();
        loadHistorySize();
    });
}

void CoreBridge::pairWith(const QString &deviceId)
{
    auto msg = methodCall(iface, QStringLiteral("PairWith"));
    msg << deviceId;
    callAsync(msg, {}, [this] { emit pairingFinished(false, QString(), i18n("The sharing service did not accept the request")); });
}

void CoreBridge::confirmPairing(bool accept)
{
    auto msg = methodCall(iface, QStringLiteral("ConfirmPairing"));
    msg << accept;
    callAsync(msg, {}, [this] { emit pairingFinished(false, QString(), i18n("The sharing service did not accept the request")); });
}

void CoreBridge::loadHistorySize()
{
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(methodCall(iface, QStringLiteral("HistorySize"))), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusPendingReply<qulonglong> reply = *w;
        if (!reply.isError() && reply.value() != m_historyBytes) {
            m_historyBytes = reply.value();
            emit historyBytesChanged();
        }
    });
}

void CoreBridge::openDownloadFolder() const
{
    const QString dir = m_downloadDir.isEmpty() ? QStandardPaths::writableLocation(QStandardPaths::DownloadLocation) : m_downloadDir;
    QDesktopServices::openUrl(QUrl::fromLocalFile(dir));
}

void CoreBridge::copyToClipboard(const QString &text) const
{
    QGuiApplication::clipboard()->setText(text);
}

void CoreBridge::openLink(const QString &url) const
{
    QDesktopServices::openUrl(QUrl(url.trimmed()));
}

bool CoreBridge::fileExists(const QString &path) const
{
    const QString dir = m_downloadDir.isEmpty() ? QStandardPaths::writableLocation(QStandardPaths::DownloadLocation) : m_downloadDir;
    return isOpenableReceivedFile(path, dir);
}

void CoreBridge::openFile(const QString &path) const
{
    if (fileExists(path)) {
        QDesktopServices::openUrl(QUrl::fromLocalFile(path));
    }
}

void CoreBridge::joinNetwork(const QString &transferId, const QString &ssid)
{
    auto msg = methodCall(iface, QStringLiteral("JoinNetwork"));
    msg << transferId;
    callAsync(msg, [this, ssid] { emit networkJoined(ssid); });
}

void CoreBridge::setVisible(bool visible)
{
    if (visible == m_visible)
    {
        return;
    }
    m_visible = visible;
    emit visibleChanged();
    setRemoteProperty(QStringLiteral("Visible"), visible);
}

void CoreBridge::makeVisibleFor(int seconds)
{
    auto msg = methodCall(iface, QStringLiteral("MakeVisibleFor"));
    msg << static_cast<quint32>(qMax(0, seconds));
    callAsync(msg);
}

void CoreBridge::onPropertiesChanged(const QString &interface, const QVariantMap &changed, const QStringList &)
{
    if (interface != iface || !changed.contains(QStringLiteral("VisibleUntil")))
    {
        return;
    }
    const qint64 until = changed.value(QStringLiteral("VisibleUntil")).toLongLong();
    if (until != m_visibleUntil)
    {
        m_visibleUntil = until;
        emit visibleUntilChanged();
    }
}

void CoreBridge::setDeviceName(const QString &name)
{
    if (name.trimmed() != m_deviceName)
    {
        setRemoteProperty(QStringLiteral("DeviceName"), name);
    }
}

void CoreBridge::setDownloadDir(const QString &dir)
{
    // In a Flatpak a folder the sandbox does not share would accept the files and lose them.
    if (!dir.isEmpty() && Launcher::inFlatpak() && !Launcher::isSharedWithHost(dir))
    {
        emit errorOccurred(i18n("The Flatpak has no access to %1. Allow it with: flatpak override --user --filesystem=\"%1\" %2", dir, Launcher::flatpakId()));
        return;
    }
    if (dir != m_downloadDir)
    {
        setRemoteProperty(QStringLiteral("DownloadDir"), dir);
    }
}

void CoreBridge::setStaticPort(int port)
{
    if (port != m_staticPort)
    {
        setRemoteProperty(QStringLiteral("StaticPort"), QVariant::fromValue(static_cast<quint32>(port)));
    }
}

void CoreBridge::setHideWhenDnd(bool hide)
{
    if (hide == m_hideWhenDnd)
    {
        return;
    }
    m_hideWhenDnd = hide;
    emit hideWhenDndChanged();
    setRemoteProperty(QStringLiteral("HideWhenDnd"), hide);
}

void CoreBridge::setSuspended(bool suspended)
{
    if (suspended == m_suspended)
    {
        return;
    }
    m_suspended = suspended;
    emit suspendedChanged();
    setRemoteProperty(QStringLiteral("Suspended"), suspended);
}

void CoreBridge::setAutostart(bool enabled)
{
    if (enabled == m_autostart)
    {
        return;
    }
    if (!Autostart::setEnabled(enabled))
    {
        emit errorOccurred(i18n("Could not update the autostart entry"));
    }
    m_autostart = Autostart::isEnabled();
    emit autostartChanged();
}

void CoreBridge::startDiscovery()
{
    m_discoveryStopTimer.stop();
    if (!m_discoveryRunning)
    {
        m_discoveryRunning = true;
        m_devices.clear();
        callAsync(methodCall(iface, QStringLiteral("StartDiscovery")), {}, [this] { m_discoveryRunning = false; });
    }
}

void CoreBridge::scanBluetooth()
{
    call(QStringLiteral("ScanBluetooth"), {});
}

void CoreBridge::stopDiscovery()
{
    m_discoveryStopTimer.start();
}

void CoreBridge::stopDiscoveryNow()
{
    m_discoveryRunning = false;
    m_devices.clear();
    call(QStringLiteral("StopDiscovery"), {});
}

void CoreBridge::sendFiles(const QString &deviceId, const QStringList &paths)
{
    m_outgoingFileCounts.insert(deviceId, paths.size());
    emit sendRequested(deviceId, m_devices.nameOf(deviceId), paths.size());
    startSend(deviceId,
              {{QStringLiteral("files"), paths.join(QLatin1Char('\n'))}},
              QStringLiteral("SendFiles"),
              {deviceId, paths},
              true);
}

void CoreBridge::startSend(const QString &deviceId, QVariantMap row, const QString &method, const QVariantList &args, bool hasJob)
{
    // Show the row now: finding the device and connecting can take several seconds before the
    // core reports anything. The first TransferUpdated for this id takes it over.
    row.insert(QStringLiteral("device"), m_devices.nameOf(deviceId));
    const quint64 number = m_transfers.beginPending(deviceId, row);
    auto msg = methodCall(iface, method);
    msg.setArguments(args);
    callAsync(msg, {}, [this, deviceId, number, hasJob]
              {
                  m_transfers.failPending(deviceId, number);
                  if (hasJob)
                  {
                      emit sendRefused(deviceId);
                  }
              });
    // The core's own watchdog (40 s over the LAN and over Bluetooth) fires first: this only keeps a row from staying at "Connecting…" for good.
    QTimer::singleShot(45'000, this, [this, deviceId, number]
                       {
                           if (m_transfers.failPending(deviceId, number))
                           {
                               emit transferChanged(deviceId);
                           }
                       });
}

void CoreBridge::sendText(const QString &deviceId, const QString &text, bool isUrl)
{
    const QString kind = isUrl ? QStringLiteral("url") : QStringLiteral("text");
    startSend(deviceId,
              {{QStringLiteral("text"), text}, {QStringLiteral("payload_kind"), kind}},
              QStringLiteral("SendText"),
              {deviceId, text, kind},
              false);
}

QVariantMap CoreBridge::fileDetails(const QString &path) const
{
    const QFileInfo info(path);
    return {
        {QStringLiteral("path"), path},
        {QStringLiteral("name"), info.fileName()},
        {QStringLiteral("bytes"), info.size()},
        {QStringLiteral("size"), formatSize(info.size())},
        {QStringLiteral("icon"), QMimeDatabase().mimeTypeForFile(info).iconName()},
    };
}

QString CoreBridge::clipboardText(int maxChars) const
{
    return QGuiApplication::clipboard()->text().left(maxChars);
}

QString CoreBridge::iconForName(const QString &name) const
{
    return QMimeDatabase().mimeTypeForFile(name, QMimeDatabase::MatchExtension).iconName();
}

QString CoreBridge::formatSize(qlonglong bytes) const
{
    return QLocale().formattedDataSize(bytes);
}

QStringList CoreBridge::expandPaths(const QStringList &paths, QStringList *problems)
{
    const SendExpansion expansion = expandForSend(paths);
    QStringList messages;
    for (const QString &folder : expansion.tooMany)
    {
        messages << i18n("The folder %1 has more than %2 files", QFileInfo(folder).fileName(), maxFolderFiles);
    }
    for (const QString &folder : expansion.empty)
    {
        messages << i18n("The folder %1 has no files to send", QFileInfo(folder).fileName());
    }
    if (problems)
    {
        *problems = messages;
    }
    else
    {
        for (const QString &message : messages)
        {
            emit errorOccurred(message);
        }
    }
    return expansion.files;
}

QStringList CoreBridge::filesFromUrls(const QList<QUrl> &urls)
{
    QStringList paths;
    bool skipped = false;
    for (const QUrl &url : urls)
    {
        const QString path = url.toLocalFile();
        const QFileInfo info(path);
        if (path.isEmpty() || !(info.isFile() || info.isDir()))
        {
            skipped = true;
        }
        else
        {
            paths << path;
        }
    }
    if (skipped)
    {
        emit errorOccurred(i18n("Only local files and folders can be shared"));
    }
    return expandPaths(paths);
}

void CoreBridge::cancel(const QString &id)
{
    if (m_transfers.cancelPending(id)) {
        emit transferChanged(id);
    }
    call(QStringLiteral("Cancel"), {id});
}

void CoreBridge::onTransferUpdated(const QString &id, const QVariantMap &received)
{
    QVariantMap info = received;
    for (const QString &key : {QStringLiteral("device"), QStringLiteral("files")}) {
        if (info.contains(key)) {
            info[key] = plain(info.value(key).toString());
        }
    }
    m_transfers.update(id, info);
    emit transferChanged(id);
    // A finished transfer was added to the saved history: its size may have crossed the warning level.
    if (TransferModel::isFinal(info.value(QStringLiteral("state")).toString())) {
        loadHistorySize();
    }
}

void CoreBridge::onDeviceFound(const QString &id, const QVariantMap &info)
{
    const QString medium = info.value(QStringLiteral("medium"), QStringLiteral("lan")).toString();
    const QString address = medium == QLatin1String("ble")
                                ? QString()
                                : QStringLiteral("%1:%2").arg(info.value(QStringLiteral("ip")).toString(), info.value(QStringLiteral("port")).toString());
    m_devices.upsert(id,
                     plain(info.value(QStringLiteral("name")).toString()),
                     info.value(QStringLiteral("device_type")).toString(),
                     address,
                     info.value(QStringLiteral("trust")).toString(),
                     medium,
                     info.value(QStringLiteral("pairing")).toBool());
}

void CoreBridge::onDeviceLost(const QString &id)
{
    m_devices.remove(id);
}
