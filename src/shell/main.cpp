#include <malloc.h>

#include <csignal>
#include <memory>

#include <QApplication>
#include <QCommandLineParser>
#include <QDateTime>
#include <QDir>
#include <QFileInfo>
#include <QIcon>
#include <QMenu>
#include <QPointer>
#include <QQmlApplicationEngine>
#include <QQuickStyle>
#include <QQuickWindow>
#include <QTimer>

#include <KAboutData>
#include <KCrash>
#include <KDBusService>
#include <KLocalizedQmlContext>
#include <KLocalizedString>
#include <KSignalHandler>
#include <KStatusNotifierItem>

#include "corebridge.h"
#include "devicematch.h"
#include "devicemodel.h"
#include "instance.h"
#include "integration.h"
#include "dndwatcher.h"
#include "transferwatcher.h"
#include "updatewatcher.h"

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);
    QApplication::setQuitOnLastWindowClosed(false);
    KCrash::initialize();
    if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE"))
    {
        QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
    }

    KLocalizedString::setApplicationDomain("kuickshare");
    KAboutData about(QStringLiteral("kuickshare"),
                     QStringLiteral("KuickShare"),
                     QStringLiteral(KUICKSHARE_VERSION),
                     i18n("Quick Share client for KDE Plasma"),
                     KAboutLicense::GPL_V3);
    about.setDesktopFileName(QStringLiteral("io.github.the_beper.KuickShare"));
    KAboutData::setApplicationData(about);
    // The app icons are installed with the application; fall back to a stock icon when running from a build tree.
    const QString appIcon = QIcon::hasThemeIcon(QStringLiteral("io.github.the_beper.KuickShare")) ? QStringLiteral("io.github.the_beper.KuickShare") : QStringLiteral("emblem-shared");
    const QString trayIcon = QIcon::hasThemeIcon(QStringLiteral("io.github.the_beper.KuickShare-symbolic")) ? QStringLiteral("io.github.the_beper.KuickShare-symbolic") : appIcon;
    QApplication::setWindowIcon(QIcon::fromTheme(appIcon));

    const QCommandLineOption background(QStringLiteral("background"), i18n("Start hidden in the system tray"));
    const QCommandLineOption page(QStringList{QStringLiteral("page")}, i18n("Open the given page: send, transfers, settings or help"), QStringLiteral("page"));
    const QCommandLineOption device(QStringLiteral("device"), i18n("Send the files to the nearby device with this name"), QStringLiteral("name"));
    const QCommandLineOption replace(QStringLiteral("replace"), i18n("Quit a running instance and take its place"));
    auto setupParser = [&](QCommandLineParser &p)
    {
        about.setupCommandLine(&p);
        p.addOption(background);
        p.addOption(replace);
        p.addOption(page);
        p.addOption(device);
        p.addPositionalArgument(QStringLiteral("files"), i18n("Files to send"), QStringLiteral("[files...]"));
    };
    QCommandLineParser parser;
    setupParser(parser);
    parser.process(app);
    about.processCommandLine(&parser);

    KSignalHandler::self()->watchSignal(SIGTERM);
    KSignalHandler::self()->watchSignal(SIGINT);
    QObject::connect(KSignalHandler::self(), &KSignalHandler::signalReceived, &app, &QApplication::quit);

    // A newer or older version left running by an update must not answer in place of this one.
    Instance::replaceRunning(parser.isSet(replace));
    KDBusService service(KDBusService::Unique);

    CoreBridge core;
    TransferWatcher transfers(&core);
    DndWatcher dnd(&core);

    std::unique_ptr<QQmlApplicationEngine> engine;

    // An update installed under a running instance: start the new version once idle (no transfer, window closed).
    UpdateWatcher updates;
    bool updatePending = false;
    bool replacing = false;
    QTimer restartWhenIdle;
    restartWhenIdle.setInterval(30000);
    auto startReplacing = [&](bool background)
    {
        if (!UpdateWatcher::startReplacement(background))
        {
            return false;
        }
        // The new instance takes this one over within seconds; if it does not, allow another try.
        replacing = true;
        QTimer::singleShot(15000, &app, [&replacing]
                           { replacing = false; });
        restartWhenIdle.stop();
        return true;
    };
    auto restartIfIdle = [&](bool background)
    {
        if (replacing)
        {
            return true;
        }
        if (updatePending && !engine && !core.transferLive())
        {
            return startReplacing(background);
        }
        return false;
    };
    QObject::connect(&restartWhenIdle, &QTimer::timeout, &app, [&]
                     { restartIfIdle(true); });
    QObject::connect(&updates, &UpdateWatcher::updateInstalled, &app, [&]
                     {
        updatePending = true;
        core.setUpdateInstalled(true);
        if (!restartIfIdle(true))
        {
            restartWhenIdle.start();
        } });

    // The "Restart" button of the update banner: the window is open, so the new instance opens it too.
    QObject::connect(&core, &CoreBridge::restartRequested, &app, [&]
                     {
        // Same condition as the button: a transfer waiting for the user's consent counts as busy.
        if (!replacing && !core.transferBusy())
        {
            startReplacing(false);
        } });

    QString startPage = parser.value(page);
    bool askIntegration = Integration::shouldAsk(parser.isSet(background));
    auto ensureWindow = [&]() -> QQuickWindow *
    {
        if (engine)
        {
            return qobject_cast<QQuickWindow *>(engine->rootObjects().constFirst());
        }
        engine = std::make_unique<QQmlApplicationEngine>();
        KLocalization::setupLocalizedContext(engine.get());
        QVariantMap initial{{QStringLiteral("core"), QVariant::fromValue(&core)}, {QStringLiteral("askIntegration"), askIntegration}};
        if (!startPage.isEmpty())
        {
            initial.insert(QStringLiteral("startPage"), startPage);
        }
        askIntegration = false;
        startPage.clear();
        engine->setInitialProperties(initial);
        engine->loadFromModule(QStringLiteral("io.github.the_beper.KuickShare"), QStringLiteral("Main"));
        if (engine->rootObjects().isEmpty())
        {
            engine.reset();
            return nullptr;
        }
        QPointer<QQuickWindow> window = qobject_cast<QQuickWindow *>(engine->rootObjects().constFirst());
        QObject::connect(window, &QQuickWindow::visibleChanged, &app, [&engine, &app, &restartIfIdle, window](bool visible)
                         {
            if (visible)
            {
                return;
            }
            // Not from inside the close handler that hid the window.
            QTimer::singleShot(0, &app, [&engine, &restartIfIdle, window]
                               {
                if (window && !window->isVisible())
                {
                    engine.reset();
#ifdef __GLIBC__
                    malloc_trim(0);
#endif
                    restartIfIdle(true);
                } }); });
        return window;
    };

    auto showWindow = [&]
    {
        if (auto *window = ensureWindow())
        {
            window->show();
            window->raise();
            window->requestActivate();
        }
    };
    // Do not open the old version over a newer installed one: the new instance opens the window and replaces this one.
    // Only for a plain open: the page, transfer or action asked for would be lost.
    auto openWindow = [&]
    {
        if (!restartIfIdle(false))
        {
            showWindow();
        }
    };
    // Set by queueFiles when a folder was refused (too many files, or none): a `--device` send must not go on.
    bool queueRefused = false;
    auto queueFiles = [&](const QStringList &paths, const QString &workingDirectory) -> QStringList
    {
        QStringList absolute;
        for (const QString &path : paths)
        {
            absolute << QDir(workingDirectory).absoluteFilePath(path);
        }
        QStringList problems;
        const QStringList files = core.expandPaths(absolute, &problems);
        queueRefused = !problems.isEmpty();
        if (!files.isEmpty() || !problems.isEmpty())
        {
            if (auto *window = ensureWindow())
            {
                if (!files.isEmpty())
                {
                    QMetaObject::invokeMethod(window, "queueFiles", Q_ARG(QVariant, QVariant(files)));
                }
                for (const QString &problem : problems)
                {
                    QMetaObject::invokeMethod(window, "showError", Q_ARG(QVariant, problem));
                }
            }
        }
        return files;
    };

    // `--device <name>`: wait for discovery to show exactly one device with that name, then send the queued files to it.
    // On any failure nothing is sent and the files stay queued on the Send page.
    // The request owns its files: the window (and its queue) may be closed while it waits.
    QString pendingDevice;
    QStringList pendingFiles;
    qint64 deviceDeadline = 0;
    // A match must stay the only one for a few seconds before the send: a twin may still show up.
    QString settleId;
    qint64 settleSince = 0;
    QTimer devicePoll;
    devicePoll.setInterval(500);
    auto endDeviceSend = [&](bool stopDiscovery)
    {
        devicePoll.stop();
        pendingDevice.clear();
        pendingFiles.clear();
        settleId.clear();
        if (stopDiscovery)
        {
            core.stopDiscovery();
        }
    };
    auto failDeviceSend = [&](const QString &reason)
    {
        // A visible Send page keeps discovery on by itself.
        const auto *window = engine ? qobject_cast<QQuickWindow *>(engine->rootObjects().constFirst()) : nullptr;
        const QStringList files = pendingFiles;
        endDeviceSend(!window || !window->isVisible());
        showWindow(); // also with --background: the error must be seen
        if (engine)
        {
            // Keep the files queued even if the window was closed meanwhile.
            QMetaObject::invokeMethod(ensureWindow(), "queueFiles", Q_ARG(QVariant, QVariant(files)));
            QMetaObject::invokeMethod(ensureWindow(), "showError", Q_ARG(QVariant, reason));
        }
    };
    auto checkDevice = [&]
    {
        if (pendingDevice.isEmpty())
        {
            return;
        }
        if (core.connected())
        {
            core.startDiscovery(); // idempotent; retries when the core was not up yet
        }
        QList<DeviceRef> refs;
        const DeviceModel *devices = core.devices();
        for (int row = 0; row < devices->rowCount(); ++row)
        {
            const QModelIndex index = devices->index(row);
            refs.append({devices->data(index, DeviceModel::IdRole).toString(), devices->data(index, DeviceModel::NameRole).toString()});
        }
        const DeviceMatch match = matchDevice(refs, pendingDevice);
        if (match.kind == DeviceMatch::Kind::Ambiguous)
        {
            failDeviceSend(i18n("Several devices are named %1", pendingDevice));
        }
        else if (match.kind == DeviceMatch::Kind::None)
        {
            settleId.clear();
            if (QDateTime::currentMSecsSinceEpoch() >= deviceDeadline)
            {
                failDeviceSend(i18n("No device named %1 found", pendingDevice));
            }
        }
        else
        {
            const qint64 now = QDateTime::currentMSecsSinceEpoch();
            if (match.id != settleId)
            {
                settleId = match.id;
                settleSince = now;
            }
            if (now - settleSince < 3000 && now < deviceDeadline)
            {
                return;
            }
            if (core.transferBusy())
            {
                failDeviceSend(i18n("A transfer is in progress. You can send again when it ends"));
                return;
            }
            const QStringList files = pendingFiles;
            endDeviceSend(false); // the Send page's own handler follows the transfer
            if (auto *window = ensureWindow())
            {
                QMetaObject::invokeMethod(window, "sendQueuedTo", Q_ARG(QVariant, match.id), Q_ARG(QVariant, QVariant(files)));
            }
        }
    };
    QObject::connect(&devicePoll, &QTimer::timeout, &app, checkDevice);
    // A new request replaces a pending one.
    auto startDeviceSend = [&](const QString &name, const QStringList &queued)
    {
        endDeviceSend(false);
        QStringList files = queued;
        if (files.isEmpty() && engine)
        {
            // Nothing new: use what is already queued on the Send page.
            files = ensureWindow()->property("outgoingFiles").toStringList();
        }
        if (files.isEmpty())
        {
            failDeviceSend(i18n("No files to send"));
            return;
        }
        pendingFiles = files;
        pendingDevice = name;
        deviceDeadline = QDateTime::currentMSecsSinceEpoch() + 30000;
        core.startDiscovery();
        devicePoll.start();
        checkDevice();
    };
    QObject::connect(&service, &KDBusService::activateRequested, &app, [&](const QStringList &arguments, const QString &workingDirectory)
                     {
        QCommandLineParser other;
        setupParser(other);
        other.parse(arguments);
        const QStringList queued = queueFiles(other.positionalArguments(), workingDirectory);
        if (other.isSet(device))
        {
            if (queueRefused)
            {
                // The error is already shown and the expanded files stay queued; nothing is sent, not even older queue contents.
                endDeviceSend(false);
                showWindow();
                return;
            }
            if (!other.isSet(background))
            {
                showWindow();
            }
            startDeviceSend(other.value(device), queued);
            return;
        }
        showWindow(); });
    const QStringList queued = queueFiles(parser.positionalArguments(), QDir::currentPath());
    if (!parser.isSet(background))
    {
        showWindow();
    }
    if (parser.isSet(device) && queueRefused)
    {
        showWindow();
    }
    else if (parser.isSet(device))
    {
        startDeviceSend(parser.value(device), queued);
    }

    KStatusNotifierItem tray;
    tray.setCategory(KStatusNotifierItem::ApplicationStatus);
    tray.setIconByName(trayIcon);
    tray.setTitle(QStringLiteral("KuickShare"));
    auto applyTray = [&tray, &core]
    {
        tray.setStatus(core.trayIcon() ? KStatusNotifierItem::Active : KStatusNotifierItem::Passive);
    };
    applyTray();
    QObject::connect(&core, &CoreBridge::trayIconChanged, &tray, applyTray);
    tray.setStandardActionsEnabled(false);
    QObject::connect(&tray, &KStatusNotifierItem::activateRequested, &app, openWindow);

    QObject::connect(&transfers, &TransferWatcher::viewRequested, &app, [&](const QString &id)
                     {
        showWindow();
        if (engine)
        {
            QMetaObject::invokeMethod(ensureWindow(), "showTransfer", Q_ARG(QVariant, id));
        } });

    auto inWindow = [&](const char *function)
    {
        return [&, function]
        {
            showWindow();
            if (engine)
            {
                QMetaObject::invokeMethod(ensureWindow(), function);
            }
        };
    };

    auto *menu = new QMenu();
    menu->addAction(QIcon::fromTheme(QStringLiteral("window-new")), i18n("Open KuickShare"), openWindow);
    menu->addSeparator();
    menu->addAction(QIcon::fromTheme(QStringLiteral("document-send")), i18n("Send files…"), inWindow("openFiles"));
    menu->addAction(QIcon::fromTheme(QStringLiteral("edit-paste")), i18n("Send clipboard"), inWindow("pasteClipboard"));
    auto *visible = menu->addAction(QIcon::fromTheme(QStringLiteral("visibility")), i18n("Visible to nearby devices"));
    visible->setCheckable(true);
    visible->setChecked(core.visible());
    QObject::connect(visible, &QAction::triggered, &core, &CoreBridge::setVisible);
    QObject::connect(&core, &CoreBridge::visibleChanged, visible, [visible, &core]
                     { visible->setChecked(core.visible()); });
    auto *visibleFor = menu->addAction(QIcon::fromTheme(QStringLiteral("chronometer")), QString());
    visibleFor->setCheckable(true);
    auto *visibleForTimer = new QTimer(visibleFor);
    visibleForTimer->setInterval(30000);
    auto updateVisibleFor = [visibleFor, &core]
    {
        const qint64 left = core.visibleUntil() - QDateTime::currentSecsSinceEpoch();
        const bool active = core.visibleUntil() > 0 && left > 0;
        visibleFor->setChecked(active);
        const int minutes = static_cast<int>((left + 59) / 60);
        visibleFor->setText(active ? i18np("Visible for %1 more minute (click to stop)", "Visible for %1 more minutes (click to stop)", minutes)
                                   : i18n("Visible for 10 minutes"));
    };
    QObject::connect(visibleForTimer, &QTimer::timeout, visibleFor, updateVisibleFor);
    QObject::connect(&core, &CoreBridge::visibleUntilChanged, visibleFor, [visibleForTimer, &core, updateVisibleFor]
                     {
        core.visibleUntil() > 0 ? visibleForTimer->start() : visibleForTimer->stop();
        updateVisibleFor(); });
    QObject::connect(visibleFor, &QAction::triggered, &core, [&core, updateVisibleFor]
                     {
        core.makeVisibleFor(core.visibleUntil() > QDateTime::currentSecsSinceEpoch() ? 0 : 600);
        updateVisibleFor(); // the core's answer fixes the state if the call fails
    });
    updateVisibleFor();
    menu->addSeparator();
    menu->addAction(QIcon::fromTheme(QStringLiteral("configure")), i18n("Preferences"), inWindow("showSettings"));

    auto requestQuit = inWindow("requestQuit");
    menu->addAction(QIcon::fromTheme(QStringLiteral("application-exit")), i18n("Quit"), [&, requestQuit]
                    {
        if (core.transferActive())
        {
            requestQuit();
        }
        else
        {
            tray.setStatus(KStatusNotifierItem::Passive);
            app.quit();
        } });
    tray.setContextMenu(menu);

    const int code = app.exec();
    // The window outlives the objects its handlers refer to otherwise.
    engine.reset();
    return code;
}
