#include "corebridge.h"

#include <QCoreApplication>
#include <QDBusConnection>
#include <QDBusConnectionInterface>
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

#include <KLocalizedString>

#include "autostart.h"
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
    bus.connect(service, path, iface, QStringLiteral("TrustedChanged"), this, SLOT(loadTrustedDevices()));

    auto *watcher = new QDBusServiceWatcher(service, bus, QDBusServiceWatcher::WatchForOwnerChange, this);
    connect(watcher, &QDBusServiceWatcher::serviceRegistered, this, &CoreBridge::onServiceRegistered);
    connect(watcher, &QDBusServiceWatcher::serviceUnregistered, this, &CoreBridge::onServiceUnregistered);

    // Stopping discovery leaks an mDNS daemon in the library, and restarting it is slow: keep it
    // running for a while after the user leaves the Send page.
    m_discoveryStopTimer.setSingleShot(true);
    m_discoveryStopTimer.setInterval(30000);
    connect(&m_discoveryStopTimer, &QTimer::timeout, this, &CoreBridge::stopDiscoveryNow);

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
                if (url.isLocalFile() && QFileInfo(url.toLocalFile()).isFile()) {
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
            if (url.isLocalFile() && QFileInfo(url.toLocalFile()).isFile()) {
                files << url.toLocalFile();
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
    refreshProperties();
    m_transfers.clearAll();
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
    m_discoveryResume = m_discoveryRunning;
    m_discoveryRunning = false;
    m_devices.clear();
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
        const bool wifiUpgrade = props.value(QStringLiteral("WifiUpgrade"), m_wifiUpgrade).toBool();
        const bool wifiChanged = wifiUpgrade != m_wifiUpgrade;
        m_wifiUpgrade = wifiUpgrade;
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
        if (dolphinChanged) {
            emit dolphinIntegrationChanged();
        }
        if (wifiChanged) {
            emit wifiUpgradeChanged();
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
    if (enabled == m_wifiUpgrade) {
        return;
    }
    m_wifiUpgrade = enabled;
    emit wifiUpgradeChanged();
    setRemoteProperty(QStringLiteral("WifiUpgrade"), enabled);
}

void CoreBridge::requestWifiUpgrade()
{
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(methodCall(iface, QStringLiteral("FirewallManaged"))), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w)
            {
        w->deleteLater();
        const QDBusPendingReply<bool> reply = *w;
        // A core that cannot answer is not a reason to refuse the setting.
        if (reply.isError() || reply.value()) {
            setWifiUpgrade(true);
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

QStringList CoreBridge::filesFromUrls(const QList<QUrl> &urls)
{
    QStringList files;
    bool skipped = false;
    for (const QUrl &url : urls)
    {
        const QString path = url.toLocalFile();
        if (!path.isEmpty() && QFileInfo(path).isFile())
        {
            files << path;
        }
        else
        {
            skipped = true;
        }
    }
    if (skipped)
    {
        emit errorOccurred(i18n("Only files can be shared, folders are not supported"));
    }
    return files;
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
