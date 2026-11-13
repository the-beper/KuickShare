#include "autostart.h"
#include "execquote.h"
#include "launcher.h"

#include <QCoreApplication>
#include <QDebug>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QProcess>
#include <QSaveFile>
#include <QStandardPaths>

namespace
{
    QString entryPath()
    {
        return Launcher::hostHome(Launcher::inFlatpak(), QStandardPaths::GenericConfigLocation) + QStringLiteral("/autostart/io.github.the_beper.KuickShare.desktop");
    }

    struct Entry
    {
        bool found = false;
        bool enabled = true;
        QString exec;
    };

    bool isTrue(const QString &value)
    {
        // What systemd-xdg-autostart-generator reads as a boolean, in its lower-case form.
        return value == QLatin1String("true") || value == QLatin1String("1") || value == QLatin1String("yes") || value == QLatin1String("on");
    }

    // Reads what decides an autostart entry: whether it exists and is enabled (`Hidden`, which
    // Plasma's Autostart page writes when an entry is switched off) and its `Exec`.
    Entry readEntry()
    {
        Entry entry;
        QFile file(entryPath());
        if (!file.exists())
        {
            return entry;
        }
        entry.found = true;
        if (!file.open(QIODevice::ReadOnly | QIODevice::Text))
        {
            return entry;
        }
        bool desktopEntry = false;
        while (!file.atEnd())
        {
            const QString line = QString::fromUtf8(file.readLine()).trimmed();
            if (line.startsWith(QLatin1Char('[')))
            {
                desktopEntry = line == QLatin1String("[Desktop Entry]");
                continue;
            }
            if (!desktopEntry || line.startsWith(QLatin1Char('#')))
            {
                continue;
            }
            const qsizetype equals = line.indexOf(QLatin1Char('='));
            if (equals < 0)
            {
                continue;
            }
            const QString key = line.left(equals).trimmed();
            const QString value = line.mid(equals + 1).trimmed().toLower();
            if (key == QLatin1String("Exec"))
            {
                entry.exec = line.mid(equals + 1).trimmed();
            }
            else if (key == QLatin1String("Hidden") && isTrue(value))
            {
                entry.enabled = false;
            }
        }
        return entry;
    }

    bool startsInBackground(const Entry &entry)
    {
        return QProcess::splitCommand(entry.exec).contains(QStringLiteral("--background"));
    }
}

bool Autostart::isEnabled()
{
    const Entry entry = readEntry();
    return entry.found && entry.enabled;
}

bool Autostart::setEnabled(bool enabled)
{
    const QString path = entryPath();
    if (!enabled)
    {
        return !QFile::exists(path) || QFile::remove(path);
    }

    if (!QDir().mkpath(QFileInfo(path).absolutePath()))
    {
        return false;
    }
    QSaveFile file(path);
    if (!file.open(QIODevice::WriteOnly | QIODevice::Text))
    {
        return false;
    }
    const QString exec = Launcher::hostExec(Launcher::inFlatpak(), quoteForExec(QCoreApplication::applicationFilePath()), QStringLiteral("--background"));
    const QString content = QStringLiteral(
                                "[Desktop Entry]\n"
                                "Type=Application\n"
                                "Name=KuickShare\n"
                                "Comment=Quick Share client for KDE Plasma\n"
                                "Exec=%1\n"
                                "Icon=io.github.the_beper.KuickShare\n"
                                "Terminal=false\n"
                                "X-KDE-autostart-after=panel\n")
                                .arg(exec);
    file.write(content.toUtf8());
    return file.commit();
}

void Autostart::repair()
{
    const Entry entry = readEntry();
    if (!entry.found || !entry.enabled || startsInBackground(entry))
    {
        return;
    }
    // A copy of the menu entry, e.g. from Plasma's Autostart page. "Start at login" means a
    // hidden start, and the rewrite also points the entry at this installation.
    if (!setEnabled(true))
    {
        qWarning() << "could not rewrite the autostart entry" << entryPath();
    }
}
