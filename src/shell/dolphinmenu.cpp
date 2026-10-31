#include "dolphinmenu.h"
#include "execquote.h"
#include "launcher.h"

#include <QCoreApplication>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QSaveFile>
#include <QStandardPaths>

namespace
{
    QString entryPath()
    {
        return Launcher::hostHome(Launcher::inFlatpak(), QStandardPaths::GenericDataLocation) + QStringLiteral("/kio/servicemenus/kuickshare-send.desktop");
    }
}

bool DolphinMenu::exists()
{
    return QFile::exists(entryPath());
}

bool DolphinMenu::sync(bool enabled)
{
    if (exists() == enabled)
    {
        return true;
    }
    // A package that ships the entry system-wide needs no copy in the home. Not in a Flatpak: its copy is in the sandbox, which Dolphin does not read.
    if (enabled && !Launcher::inFlatpak() && !QStandardPaths::locate(QStandardPaths::GenericDataLocation, QStringLiteral("kio/servicemenus/kuickshare-send.desktop")).isEmpty())
    {
        return true;
    }
    return setEnabled(enabled);
}

bool DolphinMenu::setEnabled(bool enabled)
{
    const QString path = entryPath();
    if (!enabled)
    {
        return !QFile::exists(path) || QFile::remove(path);
    }

    // The entry is the one the installer uses (data/kuickshare-send.desktop), with the running binary as Exec.
    QFile source(QStringLiteral(":/data/kuickshare-send.desktop"));
    if (!source.open(QIODevice::ReadOnly | QIODevice::Text) || !QDir().mkpath(QFileInfo(path).absolutePath()))
    {
        return false;
    }
    QByteArray content = source.readAll();
    const bool flatpak = Launcher::inFlatpak();
    const QString exec = Launcher::hostExec(flatpak, quoteForExec(QCoreApplication::applicationFilePath()), flatpak ? QStringLiteral("@@ %F @@") : QStringLiteral("%F"));
    content.replace("Exec=kuickshare %F", QByteArrayLiteral("Exec=") + exec.toUtf8());

    QSaveFile file(path);
    if (!file.open(QIODevice::WriteOnly | QIODevice::Text))
    {
        return false;
    }
    file.write(content);
    // Dolphin ignores service menus that are not executable.
    return file.commit() && QFile::setPermissions(path, QFile::permissions(path) | QFileDevice::ExeOwner | QFileDevice::ExeGroup | QFileDevice::ExeOther);
}
