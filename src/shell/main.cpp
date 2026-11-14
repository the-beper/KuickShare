#include <malloc.h>

#include <csignal>
#include <memory>

#include <QApplication>
#include <QCommandLineParser>
#include <QDateTime>
#include <QDir>
#include <QFileInfo>
#include <QIcon>
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

#include "autostart.h"
#include "corebridge.h"
#include "devicematch.h"
#include "devicemodel.h"
#include "instance.h"
#include "integration.h"
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
    QApplication::setWindowIcon(QIcon::fromTheme(appIcon));

    const QCommandLineOption background(QStringLiteral("background"), i18n("Start the sharing service and exit"));
    const QCommandLineOption page(QStringList{QStringLiteral("page")}, i18n("Open the given page: send, transfers, settings or help"), QStringLiteral("page"));
    const QCommandLineOption device(QStringLiteral("device"), i18n("Send the files to the nearby device with this name"), QStringLiteral("name"));
    const QCommandLineOption replace(QStringLiteral("replace"), i18n("Quit a running instance and take its place"));
    const QCommandLineOption openFiles(QStringLiteral("open-files"), i18n("Open the Send page with the file dialog open"));
    const QCommandLineOption clipboard(QStringLiteral("clipboard"), i18n("Open the Send page with the clipboard ready to paste"));
    const QCommandLineOption transfer(QStringList{QStringLiteral("transfer")}, i18n("Open the given transfer on the Transfers page"), QStringLiteral("id"));
    const QCommandLineOption requestQuit(QStringLiteral("request-quit"), i18n("Quit, asking first when a transfer is in progress"));
    auto setupParser = [&](QCommandLineParser &p)
    {
        about.setupCommandLine(&p);
        p.addOption(background);
        p.addOption(replace);
        p.addOption(page);
        p.addOption(device);
        p.addOption(openFiles);
        p.addOption(clipboard);
        p.addOption(transfer);
        p.addOption(requestQuit);
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

    // "Start at login" may have been added from Plasma's Autostart page, which copies the menu
    // entry: that one opens the window. Make it the hidden start the setting promises (and a
    // disabled entry stays disabled) before the core reads the setting.
    Autostart::repair();

    CoreBridge core;

    std::unique_ptr<QQmlApplicationEngine> engine;
    // The tray and the notifications belong to the core. Set when a user quit (`Qt.quit()` in the
    // window, also after `--request-quit`) or when the window went away with "Keep running in the background" off: the core is
    // stopped by `Quit` then, at the end of the event loop.
    bool quitting = false;
    bool stopBackground = false;

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
        // `Qt.quit()` in the window (Ctrl+Q, the quit dialog) is a user quit: the service stops too.
        QObject::connect(engine.get(), &QQmlEngine::quit, &app, [&quitting]
                         { quitting = true; });
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
            // Qt already says what failed; this says what it means here: the shell stays on the
            // bus without a window, and every later launch answers through it.
            qWarning() << "the window could not be created: this launch shows nothing";
            engine.reset();
            return nullptr;
        }
        QPointer<QQuickWindow> window = qobject_cast<QQuickWindow *>(engine->rootObjects().constFirst());
        // `window` by value: this local is gone when the handler runs.
        QObject::connect(window, &QQuickWindow::visibleChanged, &app, [&, window](bool visible)
                         {
            // The toast of the window replaces the end-of-transfer notifications then.
            core.setWindowOpen(visible);
            if (visible)
            {
                return;
            }
            // Not from inside the close handler that hid the window.
            QTimer::singleShot(0, &app, [&engine, &restartIfIdle, window, &core, &stopBackground, &app]
                               {
                if (window && !window->isVisible())
                {
                    // The event loop ends a moment later (app.quit below): the write must
                    // land while the process is still on the bus.
                    core.setWindowOpen(false, true);
                    engine.reset();
#ifdef __GLIBC__
                    malloc_trim(0);
#endif
                    // The shell is only the window: the background (tray, notifications) is
                    // the core's. With "Keep running in the background" off nothing is left
                    // to run: the core quits too, and this process goes out.
                    stopBackground = !core.trayIcon();
                    restartIfIdle(true);
                    app.quit();
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

    // What a fresh start and an activation with these arguments do: the same set of actions
    // both times, so a second `kuickshare --open-files` lands like the first one did. The tray
    // and the notifications belong to the core, which asks through arguments like these.
    auto handleArguments = [&](const QCommandLineParser &other, const QString &workingDirectory)
    {
        const QStringList queued = queueFiles(other.positionalArguments(), workingDirectory);
        if (other.isSet(requestQuit))
        {
            // The whole app stops: the core quits after the engine and the Wi-Fi are done.
            // A transfer in progress asks first (the dialog lives in the window; the core
            // opened this shell only to ask). `quitting` is set when the window's `Qt.quit()`
            // runs, so a cancelled dialog leaves the service alone.
            showWindow();
            if (engine)
            {
                QMetaObject::invokeMethod(ensureWindow(), "requestQuit");
            }
            else
            {
                if (!core.transferBusy())
                {
                    quitting = true;
                    app.quit();
                }
            }
            return;
        }
        if (other.isSet(device) && queueRefused)
        {
            showWindow();
            return;
        }
        if (other.isSet(device))
        {
            if (!other.isSet(background))
            {
                showWindow();
            }
            startDeviceSend(other.value(device), queued);
            return;
        }
        // The things the core (tray, notifications) and a file manager open with. `--page
        // settings` and a second tool can be combined; a plain open follows when none arrived.
        if (other.isSet(openFiles) || other.isSet(clipboard) || other.isSet(transfer)
            || other.value(page) == QLatin1String("settings"))
        {
            if (other.isSet(openFiles) || other.isSet(clipboard) || other.isSet(transfer))
            {
                showWindow();
                if (engine)
                {
                    if (other.isSet(openFiles))
                    {
                        QMetaObject::invokeMethod(ensureWindow(), "openFiles");
                    }
                    if (other.isSet(clipboard))
                    {
                        QMetaObject::invokeMethod(ensureWindow(), "pasteClipboard");
                    }
                    if (other.isSet(transfer))
                    {
                        QMetaObject::invokeMethod(ensureWindow(), "showTransfer", Q_ARG(QVariant, QVariant(other.value(transfer))));
                    }
                }
            }
            if (other.value(page) == QLatin1String("settings"))
            {
                showWindow();
                if (engine)
                {
                    QMetaObject::invokeMethod(ensureWindow(), "showSettings");
                }
            }
            return;
        }
        // `--background` with files (or a refused folder) still shows the window: it holds the
        // queue or the error, and a window never shown would keep this process alive.
        if (!other.isSet(background) || !queued.isEmpty() || queueRefused)
        {
            showWindow();
        }
    };

    QObject::connect(&service, &KDBusService::activateRequested, &app, [&](const QStringList &arguments, const QString &workingDirectory)
                     {
        QCommandLineParser other;
        setupParser(other);
        other.parse(arguments);
        handleArguments(other, workingDirectory); });
    // `--background` (autostart entry, a restart over an update): the core started in the CoreBridge constructor owns the background from here on. The shell leaves only after the core is on the bus: a fresh connection runs the side effects of it (the Dolphin entry, the autostart). Nothing else here keeps the process going.
    if (parser.isSet(background) && parser.positionalArguments().isEmpty() && !parser.isSet(device)
        && !parser.isSet(openFiles) && !parser.isSet(clipboard) && !parser.isSet(transfer)
        && !parser.isSet(requestQuit))
    {
        if (core.connected())
        {
            QTimer::singleShot(0, &app, [&app]
            { app.quit(); });
        }
        else
        {
            QObject::connect(&core, &CoreBridge::firstPropertiesReady, &app, [&app]
            { app.quit(); }, Qt::SingleShotConnection);
            QTimer::singleShot(10000, &app, [&app]
            { app.quit(); });
        }
    }
    else
    {
        handleArguments(parser, QDir::currentPath());
    }

    const int code = app.exec();
    // The window outlives the objects its handlers refer to otherwise.
    engine.reset();
    core.setWindowOpen(false, true);
    // A user quit (`--request-quit`), or a window hidden while "Keep running in the background" is off, stops the service through its `Quit`.
    if (quitting || stopBackground)
    {
        core.stopService();
    }
    return code;
}
