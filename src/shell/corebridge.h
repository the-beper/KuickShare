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
#include "requirementsreport.h"
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
    /// The trusted devices of an older version were removed and the user has not been told yet (the core keeps this until `acknowledgeTrustedDevicesReset`).
    Q_PROPERTY(bool trustedDevicesReset READ trustedDevicesReset NOTIFY trustedDevicesResetChanged)
    Q_PROPERTY(bool trayIcon READ trayIcon WRITE setTrayIcon NOTIFY trayIconChanged)
    Q_PROPERTY(bool wifiUpgrade READ wifiUpgrade WRITE setWifiUpgrade NOTIFY wifiUpgradeChanged)
    /// The user allowed the computer's hotspot to use an open firewall zone for this session (the core forgets it at restart).
    Q_PROPERTY(bool insecureHotspotAllowed READ insecureHotspotAllowed NOTIFY insecureHotspotAllowedChanged)
    Q_PROPERTY(bool dolphinIntegration READ dolphinIntegration WRITE setDolphinIntegration NOTIFY dolphinIntegrationChanged)
    Q_PROPERTY(bool appImage READ appImage CONSTANT)
    Q_PROPERTY(bool menuEntry READ menuEntry WRITE setMenuEntry NOTIFY menuEntryChanged)
    Q_PROPERTY(bool historyEnabled READ historyEnabled WRITE setHistoryEnabled NOTIFY historyEnabledChanged)
    Q_PROPERTY(int historyKeepDays READ historyKeepDays WRITE setHistoryKeepDays NOTIFY historyKeepDaysChanged)
    Q_PROPERTY(bool hideWhenDnd READ hideWhenDnd WRITE setHideWhenDnd NOTIFY hideWhenDndChanged)
    Q_PROPERTY(QVariantList trustedDevices READ trustedDevices NOTIFY trustedDevicesChanged)
    Q_PROPERTY(QVariantList allowedDevices READ allowedDevices NOTIFY allowedDevicesChanged)
    /// What the core last reported about what Wi-Fi Direct needs on this computer, ready to show: `valid`, `stale`,
    /// `error` ("", "unavailable" or "schema"), `generatedAtMs`, `ttlMs`, `packaging`, `coreVersion`, `capabilities`
    /// (`sending`, `receiving`, `joinNetwork`, each `status`, `label` and `iconName`) and `components` (`key`, `title`,
    /// `status`, `statusText`, `iconName`, and `problems`, each `text`, `hint` and `command`).
    /// A report from before a core restart is never shown as current: it is marked `stale`.
    Q_PROPERTY(QVariantMap wifiRequirements READ wifiRequirements NOTIFY wifiRequirementsChanged)
    /// A check of the requirements is waiting for the core.
    Q_PROPERTY(bool wifiRequirementsBusy READ wifiRequirementsBusy NOTIFY wifiRequirementsBusyChanged)

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
    bool trustedDevicesReset() const { return m_trustedDevicesReset; }
    bool wifiUpgrade() const { return m_wifiUpgrade; }
    bool insecureHotspotAllowed() const { return m_insecureHotspotAllowed; }
    bool dolphinIntegration() const { return m_dolphinIntegration; }
    int historyKeepDays() const { return m_historyKeepDays; }
    bool appImage() const;
    bool menuEntry() const;
    QVariantList trustedDevices() const { return m_trustedDevices; }
    QVariantList allowedDevices() const { return m_allowedDevices; }
    QVariantMap wifiRequirements() const { return m_wifiRequirements; }
    bool wifiRequirementsBusy() const { return m_wifiRequirementsBusy; }

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
    /// The same as `requestWifiUpgrade(enabled, false)`: nothing is shown as changed until the core confirms it.
    void setWifiUpgrade(bool enabled);
    /// Turns the Wi-Fi upgrade on, or emits `firewallWarning` first when firewalld is not running (the
    /// user must then agree to the hotspot using an open zone) and waits for the answer.
    Q_INVOKABLE void enableWifiUpgrade();
    /// The core's `RequestWifiUpgrade`: `enabled` is saved; `allowInsecure` allows the hotspot an open firewall zone for
    /// this session only (needs `enabled`). Any write without it takes a given consent back. The saved value is only
    /// read back from the core; when it refuses, `wifiUpgradeChanged` is emitted (so a switch shows the real value) and,
    /// if the requirements blocked it, they are loaded again (the switch is then disabled).
    Q_INVOKABLE void requestWifiUpgrade(bool enabled, bool allowInsecure);
    void setDolphinIntegration(bool enabled);
    /// AppImage only: writes or removes the application menu entry (no first-start question is asked afterwards).
    void setMenuEntry(bool enabled);
    void setHistoryKeepDays(int days);
    /// `row` is a row of the unfiltered transfer model.
    Q_INVOKABLE void removeTransfer(int row);
    void setSuspended(bool suspended);

    /// Answer an incoming transfer with the token of the consent the core last reported for it (see `answerWithToken`).
    Q_INVOKABLE void accept(const QString &id) { answerTransfer(id, QStringLiteral("accept")); }
    Q_INVOKABLE void decline(const QString &id) { answerTransfer(id, QStringLiteral("decline")); }
    /// Accepts and remembers the sender (name and the MAC address the core saw when the transfer arrived).
    Q_INVOKABLE void acceptAndTrust(const QString &id) { answerTransfer(id, QStringLiteral("accept_and_trust")); }
    /// The core's `AnswerTransfer`: `answer` is "accept", "accept_and_trust" or "decline" and `token` the one of the prompt
    /// (`consent` in the transfer info), so a prompt of an older session cannot answer a transfer that got the same id.
    /// When the core refuses, `answerRefused` or `answerNotApplied` is emitted and `errorOccurred` explains. A token of 0
    /// (the core reports no consent) is refused without asking.
    void answerWithToken(const QString &id, quint64 token, const QString &answer);
    /// The user saw the notice of `trustedDevicesReset`.
    Q_INVOKABLE void acknowledgeTrustedDevicesReset();
    /// What to tell the user about an offer the core refused (`reason` of the transfer info); empty for no reason.
    Q_INVOKABLE static QString refusalText(const QString &reason);
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
    /// Files to queue for `paths`, folders flattened (see folderexpand.h). A message for each folder refused or empty
    /// goes to `problems`, or to `errorOccurred` when that is null.
    QStringList expandPaths(const QStringList &paths, QStringList *problems = nullptr);
    /// Local files and folders among `urls`, expanded as above; reports an error if anything else was skipped.
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
    /// Asks the core what Wi-Fi Direct needs here (the core's cache is used while it is fresh); the result is
    /// `wifiRequirements`.
    Q_INVOKABLE void loadWifiRequirements() { requestWifiRequirements(false); }
    /// The same, but the core checks again (at most every few seconds).
    Q_INVOKABLE void refreshWifiRequirements() { requestWifiRequirements(true); }
    /// Puts a plain-text report of the last result on the clipboard: versions, packaging, time, statuses and reason
    /// codes; no names, addresses or other details of this computer.
    Q_INVOKABLE void copyWifiRequirementsReport();
    /// Opens a local file with the default application.
    Q_INVOKABLE void openFile(const QString &path) const;
    /// Asks the core to join the Wi-Fi network received in a finished transfer (it keeps the password).
    /// `ssid` is only for the `networkJoined` message; a failure is reported through `errorOccurred`.
    Q_INVOKABLE void joinNetwork(const QString &transferId, const QString &ssid);

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
    void trustedDevicesResetChanged();
    void wifiUpgradeChanged();
    void wifiRequirementsChanged();
    void wifiRequirementsBusyChanged();
    void insecureHotspotAllowedChanged();
    void firewallWarning();
    /// The firewall did not allow the hotspot, so the transfer stays on Bluetooth. `reason` is a code of the core
    /// (`firewall_denied`, `firewall_missing`); unknown codes are possible.
    void wifiUpgradeBlocked(const QString &reason);
    void dolphinIntegrationChanged();
    void menuEntryChanged();
    void historyKeepDaysChanged();
    void trustedDevicesChanged();
    void allowedDevicesChanged();
    void pairingCode(const QString &code, const QString &peer);
    void pairingFinished(bool ok, const QString &peer, const QString &message);
    /// A request to the core failed; the message is meant for the user.
    void errorOccurred(const QString &message);
    void networkJoined(const QString &ssid);
    void transferChanged(const QString &id);
    /// A send of files was requested; the transfer starts later (scan, connection, consent).
    void sendRequested(const QString &deviceId, const QString &deviceName, int fileCount);
    /// The core refused the send request before a transfer existed.
    void sendRefused(const QString &deviceId);
    /// The core did not take an answer because nothing waits for it any more (the transfer is gone, was answered, or
    /// the time was over): its prompt is stale.
    void answerRefused(const QString &id);
    /// The core did not take an answer but the transfer still waits (the sender cannot be trusted): the prompt stays.
    void answerNotApplied(const QString &id);
    /// The transfers were forgotten because a core (re)started: nothing a prompt says about them holds any more.
    void transfersReset();

private Q_SLOTS:
    void onTransferUpdated(const QString &id, const QVariantMap &received);
    void onDeviceFound(const QString &id, const QVariantMap &info);
    void onDeviceLost(const QString &id);
    void onPropertiesChanged(const QString &interface, const QVariantMap &changed, const QStringList &invalidated);
    void onPairingCode(const QString &code, const QString &peer) { emit pairingCode(code, plain(peer)); }
    void onPairingFinished(bool ok, const QString &peer, const QString &message) { emit pairingFinished(ok, plain(peer), plain(message)); }
    void onWifiUpgradeBlocked(const QString &reason) { emit wifiUpgradeBlocked(reason); }
    void onWifiRequirementsSignal(const QString &json);
    void loadTrustedDevices();
    void loadAllowedDevices();
    void onServiceRegistered();
    void onServiceUnregistered();

private:
    /// Adds the pending row for a send and calls `method`; `hasJob` is true when the shell tracks a Plasma job for it.
    void startSend(const QString &deviceId, QVariantMap row, const QString &method, const QVariantList &args, bool hasJob);
    void call(const QString &method, const QVariantList &args);
    /// `answerWithToken` with the token the transfer model has for `id`.
    void answerTransfer(const QString &id, const QString &answer);
    void callAsync(const QDBusMessage &message, const std::function<void()> &onSuccess = {}, const std::function<void()> &onError = {});
    void setRemoteProperty(const QString &name, const QVariant &value);
    void refreshProperties();
    void requestWifiRequirements(bool force);
    /// Shows `report` as the current result and schedules the moment it goes stale.
    void adoptRequirements(const RequirementsReport::Parsed &report);
    /// The report on show (if any) is no longer current; `error` is "", "unavailable" or "schema".
    void markRequirementsStale(const QString &error);
    void rebuildRequirementsMap();
    void setRequirementsBusy(bool busy);
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
    bool m_insecureHotspotAllowed = false;
    bool m_dolphinIntegration = true;
    bool m_dolphinSyncFailed = false;
    bool m_trustedDevicesEnabled = true;
    bool m_trustedDevicesReset = false;
    int m_historyKeepDays = 0;
    QVariantList m_trustedDevices;
    QVariantList m_allowedDevices;
    bool m_suspended = false;
    qint64 m_visibleUntil = 0;
    int m_staticPort = 0;
    QString m_deviceName;
    QString m_downloadDir;
    QVariantMap m_wifiRequirements;
    bool m_wifiRequirementsBusy = false;
    RequirementsReport::RequestTracker m_requirementsTracker;
    RequirementsReport::Parsed m_requirementsReport;
    bool m_requirementsHasReport = false;
    bool m_requirementsStale = true;
    QString m_requirementsError;
    QTimer m_requirementsStaleTimer;
};
