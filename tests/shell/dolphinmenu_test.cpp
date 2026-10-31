#include <QFile>
#include <QFileInfo>
#include <QTemporaryDir>
#include <QtTest>

#include "dolphinmenu.h"

class DolphinMenuTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void init()
    {
        // The entry goes to $XDG_DATA_HOME/kio/servicemenus: use a scratch directory.
        QVERIFY(m_dir.isValid());
        qputenv("XDG_DATA_HOME", m_dir.path().toUtf8());
    }

    void enablingWritesAnExecutableEntryForThisBinary()
    {
        QVERIFY(!DolphinMenu::exists());
        QVERIFY(DolphinMenu::setEnabled(true));
        QVERIFY(DolphinMenu::exists());

        const QString path = m_dir.path() + QStringLiteral("/kio/servicemenus/kuickshare-send.desktop");
        QVERIFY(QFileInfo(path).isExecutable());
        QFile file(path);
        QVERIFY(file.open(QIODevice::ReadOnly | QIODevice::Text));
        const QByteArray content = file.readAll();
        QVERIFY(content.contains("Actions=sendWithKuickShare"));
        QVERIFY(content.contains("Exec=\"" + QCoreApplication::applicationFilePath().toUtf8() + "\" %F"));
    }

    void disablingRemovesTheEntryAndIsRepeatable()
    {
        QVERIFY(DolphinMenu::setEnabled(true));
        QVERIFY(DolphinMenu::setEnabled(false));
        QVERIFY(!DolphinMenu::exists());
        // Nothing to remove the second time is not an error.
        QVERIFY(DolphinMenu::setEnabled(false));
    }

    void syncMakesTheEntryMatchTheSetting()
    {
        QVERIFY(DolphinMenu::sync(true));
        QVERIFY(DolphinMenu::exists());
        QVERIFY(DolphinMenu::sync(true));
        QVERIFY(DolphinMenu::sync(false));
        QVERIFY(!DolphinMenu::exists());
        QVERIFY(DolphinMenu::sync(false));
    }

    void syncLeavesAPackagedEntryAlone()
    {
        QTemporaryDir system;
        QVERIFY(system.isValid());
        QVERIFY(QDir().mkpath(system.path() + QStringLiteral("/kio/servicemenus")));
        QVERIFY(QFile(system.path() + QStringLiteral("/kio/servicemenus/kuickshare-send.desktop")).open(QIODevice::WriteOnly));
        qputenv("XDG_DATA_DIRS", system.path().toUtf8());
        QVERIFY(DolphinMenu::sync(true));
        QVERIFY(!DolphinMenu::exists());
        qunsetenv("XDG_DATA_DIRS");
    }

    void syncReportsAnUnwritableHome()
    {
        // A file where the "kio" folder should go.
        QTemporaryDir home;
        QVERIFY(home.isValid());
        qputenv("XDG_DATA_HOME", home.path().toUtf8());
        QVERIFY(QFile(home.path() + QStringLiteral("/kio")).open(QIODevice::WriteOnly));
        QVERIFY(!DolphinMenu::sync(true));
    }

private:
    QTemporaryDir m_dir;
};

QTEST_GUILESS_MAIN(DolphinMenuTest)
#include "dolphinmenu_test.moc"
