#include "updatewatcher.h"

#include "launcher.h"
#include "portalspawn.h"

#include <QCoreApplication>
#include <QDBusArgument>
#include <QDBusConnection>
#include <QDBusMessage>
#include <QDBusObjectPath>
#include <QDBusReply>
#include <QDBusUnixFileDescriptor>
#include <QFile>
#include <QFileInfo>
#include <QDebug>
#include <QProcess>
#include <QProcessEnvironment>

#include <sys/stat.h>

namespace
{
    const QString portalService = QStringLiteral("org.freedesktop.portal.Flatpak");
    const QString portalPath = QStringLiteral("/org/freedesktop/portal/Flatpak");
    const QString portalInterface = QStringLiteral("org.freedesktop.portal.Flatpak");
    constexpr uint spawnLatestVersion = 2;
}

UpdateWatcher::UpdateWatcher(QObject *parent)
    : QObject(parent)
{
    if (Launcher::inFlatpak())
    {
        auto bus = QDBusConnection::sessionBus();
        auto create = QDBusMessage::createMethodCall(portalService, portalPath, portalInterface, QStringLiteral("CreateUpdateMonitor"));
        create << QVariantMap();
        const QDBusReply<QDBusObjectPath> monitor = bus.call(create, QDBus::Block, 2000);
        if (monitor.isValid())
        {
            bus.connect(portalService, monitor.value().path(), QStringLiteral("org.freedesktop.portal.Flatpak.UpdateMonitor"), QStringLiteral("UpdateAvailable"), this, SLOT(onFlatpakUpdate(QVariantMap)));
        }
    }
    else if (Launcher::inAppImage())
    {
        watchAppImage();
    }
}

void UpdateWatcher::onFlatpakUpdate(const QVariantMap &info)
{
    // The portal also reports a commit only available on the remote; act once it is installed.
    const QString local = info.value(QStringLiteral("local-commit")).toString();
    const QString running = info.value(QStringLiteral("running-commit")).toString();
    qInfo() << "Flatpak update available, local commit" << local << "running commit" << running;
    if (!local.isEmpty() && local != running)
    {
        emit updateInstalled();
    }
}

QString UpdateWatcher::fileSignature(const QString &path)
{
    struct stat st;
    if (::stat(QFile::encodeName(path).constData(), &st) != 0)
    {
        return {};
    }
    return QStringLiteral("%1:%2.%3").arg(st.st_ino).arg(st.st_mtim.tv_sec).arg(st.st_mtim.tv_nsec);
}

void UpdateWatcher::watchAppImage()
{
    m_appImage = Launcher::appImagePath();
    m_signature = fileSignature(m_appImage);
    if (m_signature.isEmpty())
    {
        return;
    }
    // A download rewrites the file in pieces and a tool may replace it by renaming: wait until it is quiet.
    m_settle.setSingleShot(true);
    m_settle.setInterval(5000);
    connect(&m_settle, &QTimer::timeout, this, [this]
            {
        const QString now = fileSignature(m_appImage);
        if (!now.isEmpty() && now != m_signature)
        {
            qInfo() << "AppImage replaced on disk";
            emit updateInstalled();
        } });
    auto changed = [this]
    {
        // A rename drops the watch on the old inode: watch the new file again.
        if (!m_files.files().contains(m_appImage) && QFileInfo::exists(m_appImage))
        {
            m_files.addPath(m_appImage);
        }
        m_settle.start();
    };
    connect(&m_files, &QFileSystemWatcher::fileChanged, this, changed);
    connect(&m_files, &QFileSystemWatcher::directoryChanged, this, changed);
    m_files.addPath(m_appImage);
    m_files.addPath(QFileInfo(m_appImage).absolutePath());
}

bool UpdateWatcher::startReplacement(bool background)
{
    QStringList arguments{QStringLiteral("--replace")};
    if (background)
    {
        arguments.prepend(QStringLiteral("--background"));
    }
    if (Launcher::inFlatpak())
    {
        // The portal reads argv as NUL-terminated byte strings (`aay`; QByteArrayList does not
        // marshal: the smoke test caught it, see portalspawn.h).
        QStringList items{QCoreApplication::applicationFilePath()};
        for (const QString &argument : arguments)
        {
            items << argument;
        }
        const QVariant argv = PortalSpawn::byteArgv(items);
        auto spawn = QDBusMessage::createMethodCall(portalService, portalPath, portalInterface, QStringLiteral("Spawn"));
        spawn << QByteArray("/\0", 2) << argv << PortalSpawn::emptyMap(QMetaType::fromType<uint>(), QMetaType::fromType<QDBusUnixFileDescriptor>()) << PortalSpawn::emptyMap(QMetaType::fromType<QString>(), QMetaType::fromType<QString>()) << spawnLatestVersion << QVariantMap();
        const QDBusMessage reply = QDBusConnection::sessionBus().call(spawn, QDBus::Block, 5000);
        if (reply.type() != QDBusMessage::ReplyMessage)
        {
            qWarning() << "Flatpak portal Spawn failed:" << reply.errorName() << reply.errorMessage();
            return false;
        }
        qInfo() << "Started the installed version through the Flatpak portal";
        return true;
    }
    if (Launcher::inAppImage())
    {
        // The running AppImage's variables point at its own mount; the new one sets them again.
        QProcess process;
        auto environment = QProcessEnvironment::systemEnvironment();
        environment.remove(QStringLiteral("ARGV0"));
        environment.remove(QStringLiteral("APPDIR"));
        process.setProgram(Launcher::appImagePath());
        process.setArguments(arguments);
        process.setProcessEnvironment(environment);
        return process.startDetached();
    }
    return false;
}
