#pragma once

#include <QDBusMessage>
#include <QObject>
#include <QHash>
#include <QProcess>
#include <QQmlEngine>
#include <QString>
#include <QTimer>
#include <QStringList>
#include <QUrl>
#include <QVariantMap>
#include <functional>

#include "devicemodel.h"
#include "transferfiltermodel.h"
#include "transfermodel.h"

class CoreBridge : public QObject
{
    Q_OBJECT
    Q_PROPERTY(TransferModel *transfers READ transfers CONSTANT)
    /// `transfers` narrowed by the filters of the Transfers page.
    Q_PROPERTY(TransferFilterModel *filteredTransfers READ filteredTransfers CONSTANT)
    Q_PROPERTY(DeviceModel *devices READ devices CONSTANT)
    Q_PROPERTY(bool connected READ connected NOTIFY connectedChanged)
    /// A transfer is live: the Send page waits for it to end before starting another.
    Q_PROPERTY(bool transferBusy READ transferBusy NOTIFY transferBusyChanged)
    Q_PROPERTY(bool suspended READ suspended NOTIFY suspendedChanged)
    Q_PROPERTY(bool visible READ visible WRITE setVisible NOTIFY visibleChanged)
    /// Unix seconds when a temporary visibility ends; 0 when none is active.
    Q_PROPERTY(qint64 visibleUntil READ visibleUntil NOTIFY visibleUntilChanged)
    Q_PROPERTY(QString deviceName READ deviceName WRITE setDeviceName NOTIFY deviceNameChanged)
    Q_PROPERTY(QString downloadDir READ downloadDir WRITE setDownloadDir NOTIFY downloadDirChanged)
    Q_PROPERTY(int staticPort READ staticPort WRITE setStaticPort NOTIFY staticPortChanged)
    Q_PROPERTY(bool autostart READ autostart WRITE setAutostart NOTIFY autostartChanged)
    Q_PROPERTY(QString version READ version CONSTANT)
    Q_PROPERTY(QString qtVersion READ qtVersion CONSTANT)
    Q_PROPERTY(QString frameworksVersion READ frameworksVersion CONSTANT)
    Q_PROPERTY(bool clipboardHasText READ clipboardHasText NOTIFY clipboardChanged)
    Q_PROPERTY(bool clipboardHasFiles READ clipboardHasFiles NOTIFY clipboardChanged)
    Q_PROPERTY(bool serviceStarting READ serviceStarting NOTIFY serviceStartingChanged)
    Q_PROPERTY(bool updateInstalled READ updateInstalled NOTIFY updateInstalledChanged)
    Q_PROPERTY(qulonglong historyBytes READ historyBytes NOTIFY historyBytesChanged)
    Q_PROPERTY(bool trustedDevicesEnabled READ trustedDevicesEnabled WRITE setTrustedDevicesEnabled NOTIFY trustedDevicesEnabledChanged)
    Q_PROPERTY(bool trayIcon READ trayIcon WRITE setTrayIcon NOTIFY trayIconChanged)
    Q_PROPERTY(bool wifiUpgrade READ wifiUpgrade WRITE setWifiUpgrade NOTIFY wifiUpgradeChanged)
    Q_PROPERTY(bool dolphinIntegration READ dolphinIntegration WRITE setDolphinIntegration NOTIFY dolphinIntegrationChanged)
    Q_PROPERTY(bool appImage READ appImage CONSTANT)
    Q_PROPERTY(bool menuEntry READ menuEntry WRITE setMenuEntry NOTIFY menuEntryChanged)
    Q_PROPERTY(bool historyEnabled READ historyEnabled WRITE setHistoryEnabled NOTIFY historyEnabledChanged)
    Q_PROPERTY(int historyKeepDays READ historyKeepDays WRITE setHistoryKeepDays NOTIFY historyKeepDaysChanged)
    Q_PROPERTY(bool hideWhenDnd READ hideWhenDnd WRITE setHideWhenDnd NOTIFY hideWhenDndChanged)
    Q_PROPERTY(QVariantList trustedDevices READ trustedDevices NOTIFY trustedDevicesChanged)
    Q_PROPERTY(QVariantList allowedDevices READ allowedDevices NOTIFY allowedDevicesChanged)

public:
    explicit CoreBridge(QObject *parent = nullptr);
    ~CoreBridge() override;

    TransferModel *transfers() { return &m_transfers; }
    TransferFilterModel *filteredTransfers() { return &m_filteredTransfers; }
    bool transferBusy() const { return m_transfers.busy(); }
    DeviceModel *devices() { return &m_devices; }
    bool connected() const { return m_connected; }
    bool visible() const { return m_visible; }
    bool suspended() const { return m_suspended; }
    qint64 visibleUntil() const { return m_visibleUntil; }
    QString deviceName() const { return m_deviceName; }
    QString downloadDir() const { return m_downloadDir; }
    int staticPort() const { return m_staticPort; }
    bool autostart() const { return m_autostart; }
    bool hideWhenDnd() const { return m_hideWhenDnd; }
    bool historyEnabled() const { return m_historyEnabled; }
    bool trayIcon() const { return m_trayIcon; }
    QString version() const { return QStringLiteral(KUICKSHARE_VERSION); }
    /// The versions of the libraries this app is running with (the system's, not the build container's).
    QString qtVersion() const;
    QString frameworksVersion() const;
    bool serviceStarting() const { return m_serviceStarting; }
    bool updateInstalled() const { return m_updateInstalled; }
    /// A newer version was installed under this running one (set by main.cpp).
    void setUpdateInstalled(bool installed);
    bool clipboardHasText() const { return m_clipboardHasText; }
    bool clipboardHasFiles() const { return m_clipboardHasFiles; }
    qulonglong historyBytes() const { return m_historyBytes; }
    bool trustedDevicesEnabled() const { return m_trustedDevicesEnabled; }
    bool wifiUpgrade() const { return m_wifiUpgrade; }
    bool dolphinIntegration() const { return m_dolphinIntegration; }
    int historyKeepDays() const { return m_historyKeepDays; }
    bool appImage() const;
    bool menuEntry() const;
    QVariantList trustedDevices() const { return m_trustedDevices; }
    QVariantList allowedDevices() const { return m_allowedDevices; }

    void setVisible(bool visible);
    /// Visible for `seconds` whatever `visible` says; 0 cancels.
    Q_INVOKABLE void makeVisibleFor(int seconds);
    void setDeviceName(const QString &name);
    void setDownloadDir(const QString &dir);
    void setStaticPort(int port);
    void setAutostart(bool enabled);
    void setHideWhenDnd(bool hide);
    void setHistoryEnabled(bool enabled);
    void setTrayIcon(bool show);
    void setTrustedDevicesEnabled(bool enabled);
    void setWifiUpgrade(bool enabled);
    /// Turns the Wi-Fi upgrade on, or emits `firewallWarning` first when firewalld is not running (the
    /// hotspot would then use a zone that lets the phone reach every port) and waits for the user.
    Q_INVOKABLE void requestWifiUpgrade();
    void setDolphinIntegration(bool enabled);
    /// AppImage only: writes or removes the application menu entry (no first-start question is asked afterwards).
    void setMenuEntry(bool enabled);
    void setHistoryKeepDays(int days);
    /// `row` is a row of the unfiltered transfer model.
    Q_INVOKABLE void removeTransfer(int row);
    void setSuspended(bool suspended);

    Q_INVOKABLE void accept(const QString &id) { call(QStringLiteral("Accept"), {id}); }
    Q_INVOKABLE void decline(const QString &id) { call(QStringLiteral("Decline"), {id}); }
    /// Asks the core to cancel; a send the core has not reported yet ends as Cancelled right away.
    Q_INVOKABLE void cancel(const QString &id);

    Q_INVOKABLE void startDiscovery();
    Q_INVOKABLE void stopDiscovery();
    /// One more Bluetooth scan window while discovery is on.
    Q_INVOKABLE void scanBluetooth();
    Q_INVOKABLE void sendFiles(const QString &deviceId, const QStringList &paths);
    /// How many files the last send to this device contained (0 if unknown).
    int outgoingFileCount(const QString &deviceId) const { return m_outgoingFileCounts.value(deviceId); }
    Q_INVOKABLE void sendText(const QString &deviceId, const QString &text, bool isUrl);

    Q_INVOKABLE QString toLocalFile(const QUrl &url) const { return url.toLocalFile(); }
    /// Local regular files among `urls`; reports an error if anything else (e.g. a folder) was skipped.
    Q_INVOKABLE QStringList filesFromUrls(const QList<QUrl> &urls);
    /// Display data of a local file: path, name, size (text and bytes) and a MIME icon name.
    Q_INVOKABLE QVariantMap fileDetails(const QString &path) const;
    Q_INVOKABLE QString iconForName(const QString &name) const;
    /// Text chosen by someone else (a device name, a file name) made safe to show: Qt labels render
    /// anything that looks like HTML as rich text, so "<" and ">" are replaced by look-alikes.
    static QString plain(const QString &text);
    /// The text on the clipboard, cut to `maxChars` (so a huge copy never reaches the QML text box whole).
    Q_INVOKABLE QString clipboardText(int maxChars) const;
    Q_INVOKABLE QString formatSize(qlonglong bytes) const;

    /// While open, other KuickShare computers can start a pairing with this one.
    Q_INVOKABLE void setPairing(bool open) { call(QStringLiteral("SetPairing"), {open}); }
    Q_INVOKABLE void pairWith(const QString &deviceId);
    Q_INVOKABLE void confirmPairing(bool accept);
    Q_INVOKABLE void unpair(const QString &fingerprint) { call(QStringLiteral("Unpair"), {fingerprint}); }
    /// Always accept the sender of this transfer (matched by name and MAC address).
    Q_INVOKABLE void allowDevice(const QString &transferId);
    Q_INVOKABLE void forgetAllowed(const QString &name, const QString &mac);

    Q_INVOKABLE void clearHistory();
    Q_INVOKABLE void startService();
    Q_INVOKABLE bool transferActive() const;
    /// Any transfer not finished, even one waiting for approval: an update must not restart the app meanwhile.
    bool transferLive() const;
    /// Asks main.cpp to start the installed version in place of this one.
    Q_INVOKABLE void restartForUpdate() { emit restartRequested(); }
    /// Puts every setting back to its default (history, paired and trusted devices stay).
    Q_INVOKABLE void resetSettings();
    /// The AppImage's first-launch question, "Add": the application menu entry and the Dolphin entry.
    Q_INVOKABLE void integrateAppImage();
    /// The AppImage's first-launch question, "Not now": no Dolphin entry, and the setting shows it.
    Q_INVOKABLE void declineIntegration();
    /// Files on the clipboard (copied in a file manager), or an image on it saved as a PNG: their paths.
    Q_INVOKABLE QStringList clipboardFiles() const;
    Q_INVOKABLE void openDownloadFolder() const;
    Q_INVOKABLE void copyToClipboard(const QString &text) const;
    Q_INVOKABLE void openLink(const QString &url) const;
    /// True when `path` may be opened: a plain, non-executable file inside the download folder.
    Q_INVOKABLE bool fileExists(const QString &path) const;
    /// Opens a local file with the default application.
    Q_INVOKABLE void openFile(const QString &path) const;

Q_SIGNALS:
    void connectedChanged();
    void transferBusyChanged();
    /// A live transfer finished, failed or was cancelled (not emitted for the saved history).
    void transferEnded(const QString &id, const QString &state);
    void serviceStartingChanged();
    void updateInstalledChanged();
    void restartRequested();
    void clipboardChanged();
    void historyBytesChanged();
    void visibleChanged();
    void visibleUntilChanged();
    void suspendedChanged();
    void deviceNameChanged();
    void downloadDirChanged();
    void staticPortChanged();
    void autostartChanged();
    void hideWhenDndChanged();
    void historyEnabledChanged();
    void trayIconChanged();
    void trustedDevicesEnabledChanged();
    void wifiUpgradeChanged();
    void firewallWarning();
    void dolphinIntegrationChanged();
    void menuEntryChanged();
    void historyKeepDaysChanged();
    void trustedDevicesChanged();
    void allowedDevicesChanged();
    void pairingCode(const QString &code, const QString &peer);
    void pairingFinished(bool ok, const QString &peer, const QString &message);
    /// A request to the core failed; the message is meant for the user.
    void errorOccurred(const QString &message);
    void transferChanged(const QString &id);
    /// A send of files was requested; the transfer starts later (scan, connection, consent).
    void sendRequested(const QString &deviceId, const QString &deviceName, int fileCount);
    /// The core refused the send request before a transfer existed.
    void sendRefused(const QString &deviceId);

private Q_SLOTS:
    void onTransferUpdated(const QString &id, const QVariantMap &received);
    void onDeviceFound(const QString &id, const QVariantMap &info);
    void onDeviceLost(const QString &id);
    void onPropertiesChanged(const QString &interface, const QVariantMap &changed, const QStringList &invalidated);
    void onPairingCode(const QString &code, const QString &peer) { emit pairingCode(code, plain(peer)); }
    void onPairingFinished(bool ok, const QString &peer, const QString &message) { emit pairingFinished(ok, plain(peer), plain(message)); }
    void loadTrustedDevices();
    void loadAllowedDevices();
    void onServiceRegistered();
    void onServiceUnregistered();

private:
    /// Adds the pending row for a send and calls `method`; `hasJob` is true when the shell tracks a Plasma job for it.
    void startSend(const QString &deviceId, QVariantMap row, const QString &method, const QVariantList &args, bool hasJob);
    void call(const QString &method, const QVariantList &args);
    void callAsync(const QDBusMessage &message, const std::function<void()> &onSuccess = {}, const std::function<void()> &onError = {});
    void setRemoteProperty(const QString &name, const QVariant &value);
    void refreshProperties();
    void loadHistory();
    void startCore();
    void loadHistorySize();
    void updateClipboard();
    void stopDiscoveryNow();

    TransferModel m_transfers;
    TransferFilterModel m_filteredTransfers{&m_transfers};
    DeviceModel m_devices;
    QProcess m_core;
    QTimer m_discoveryStopTimer;
    QHash<QString, int> m_outgoingFileCounts;
    bool m_discoveryRunning = false;
    bool m_discoveryResume = false; // discovery was on when the core went away
    bool m_connected = false;
    bool m_serviceStarting = false;
    bool m_updateInstalled = false;
    bool m_clipboardHasText = false;
    bool m_clipboardHasFiles = false;
    qulonglong m_historyBytes = 0;
    bool m_visible = true;
    bool m_autostart = false;
    bool m_hideWhenDnd = true;
    bool m_historyEnabled = true;
    // Off until the core says otherwise: the tray icon stays hidden meanwhile.
    bool m_trayIcon = false;
    bool m_wifiUpgrade = false;
    bool m_dolphinIntegration = true;
    bool m_trustedDevicesEnabled = true;
    int m_historyKeepDays = 0;
    QVariantList m_trustedDevices;
    QVariantList m_allowedDevices;
    bool m_suspended = false;
    qint64 m_visibleUntil = 0;
    int m_staticPort = 0;
    QString m_deviceName;
    QString m_downloadDir;
};
