#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QTemporaryDir>
#include <QtTest>

#include "autostart.h"
#include "execquote.h"
#include "launcher.h"

class AutostartTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void init()
    {
        QVERIFY(m_dir.isValid());
        // The entry goes to $XDG_CONFIG_HOME/autostart: use a scratch directory.
        qputenv("XDG_CONFIG_HOME", m_dir.path().toUtf8());
        // The test binary is not an AppImage; an inherited APPIMAGE would point the entry at one.
        qunsetenv("APPIMAGE");
        qunsetenv("APPDIR");
    }

    void cleanup()
    {
        QDir(m_dir.path()).removeRecursively();
        QDir().mkpath(m_dir.path());
    }

    void startsOffWithoutAnEntry()
    {
        QVERIFY(!Autostart::isEnabled());
        // Repairing nothing creates nothing: only the switch turns autostart on.
        Autostart::repair();
        QVERIFY(!QFileInfo::exists(entryPath()));
    }

    void theSwitchWritesAHiddenStartAndRemovesItAgain()
    {
        QVERIFY(Autostart::setEnabled(true));
        QVERIFY(Autostart::isEnabled());
        QVERIFY(content().contains(expectedExecLine()));

        QVERIFY(Autostart::setEnabled(false));
        QVERIFY(!Autostart::isEnabled());
        QVERIFY(!QFileInfo::exists(entryPath()));
        // Nothing to remove the second time is not an error.
        QVERIFY(Autostart::setEnabled(false));
    }

    void repairTurnsAPlasmaAutostartCopyIntoAHiddenStart()
    {
        // Plasma's Autostart page copies the menu entry, Exec and all: it opens the window.
        write("[Desktop Entry]\n"
              "Categories=Network;FileTransfer;Qt;KDE;\n"
              "Comment=Share files with nearby devices\n"
              "Exec=/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=kuickshare io.github.the_beper.KuickShare\n"
              "Icon=io.github.the_beper.KuickShare\n"
              "Name=KuickShare\n"
              "Type=Application\n"
              "X-Flatpak=io.github.the_beper.KuickShare\n");
        QVERIFY(Autostart::isEnabled());

        Autostart::repair();

        QVERIFY(Autostart::isEnabled());
        QVERIFY(content().contains(expectedExecLine()));
    }

    void repairLeavesAHiddenStartAlone()
    {
        QVERIFY(Autostart::setEnabled(true));
        const QByteArray before = content();
        Autostart::repair();
        QCOMPARE(content(), before);
    }

    void aDisabledEntryIsOffAndStaysDisabled()
    {
        // Plasma's Autostart page disables an entry with Hidden=true instead of removing it.
        // The app shows the switch off and must not turn the entry back on by itself.
        const QByteArray disabled = "[Desktop Entry]\n"
                                    "Type=Application\n"
                                    "Name=KuickShare\n"
                                    "Exec=kuickshare\n"
                                    "Hidden=true\n";
        write(disabled);
        QVERIFY(!Autostart::isEnabled());

        Autostart::repair();

        QCOMPARE(content(), disabled);
        // Its switch writes the background entry over the disabled one.
        QVERIFY(Autostart::setEnabled(true));
        QVERIFY(Autostart::isEnabled());
        QVERIFY(!content().contains("Hidden=true"));
        QVERIFY(content().contains(expectedExecLine()));
    }

private:
    QString entryPath() const
    {
        return m_dir.path() + QStringLiteral("/autostart/io.github.the_beper.KuickShare.desktop");
    }

    QByteArray content() const
    {
        QFile file(entryPath());
        return file.open(QIODevice::ReadOnly) ? file.readAll() : QByteArray();
    }

    void write(const QByteArray &content) const
    {
        QVERIFY(QDir().mkpath(m_dir.path() + QStringLiteral("/autostart")));
        QFile file(entryPath());
        QVERIFY(file.open(QIODevice::WriteOnly));
        QCOMPARE(file.write(content), content.size());
    }

    QString expectedExec() const
    {
        return Launcher::hostExec(false, quoteForExec(QCoreApplication::applicationFilePath()), QStringLiteral("--background"));
    }

    QByteArray expectedExecLine() const
    {
        return "Exec=" + expectedExec().toUtf8() + "\n";
    }

    QTemporaryDir m_dir;
};

QTEST_GUILESS_MAIN(AutostartTest)
#include "autostart_test.moc"
