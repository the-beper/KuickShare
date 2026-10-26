#include <QtTest>

#include "openablefile.h"

class OpenableFileTest : public QObject
{
    Q_OBJECT

private:
    static QString make(const QDir &dir, const QString &name, QFileDevice::Permissions perms = QFileDevice::ReadOwner | QFileDevice::WriteOwner)
    {
        const QString path = dir.filePath(name);
        QFile file(path);
        if (file.open(QIODevice::WriteOnly)) {
            file.write("x");
            file.close();
        }
        QFile::setPermissions(path, perms);
        return path;
    }

private Q_SLOTS:
    void acceptsAPlainFileInTheDownloadDir()
    {
        QTemporaryDir dir;
        QVERIFY(isOpenableReceivedFile(make(QDir(dir.path()), QStringLiteral("a.txt")), dir.path()));
    }

    void refusesMissingOutsideAndDirectories()
    {
        QTemporaryDir dir;
        QTemporaryDir other;
        QVERIFY(!isOpenableReceivedFile(dir.filePath(QStringLiteral("missing.txt")), dir.path()));
        QVERIFY(!isOpenableReceivedFile(make(QDir(other.path()), QStringLiteral("a.txt")), dir.path()));
        QVERIFY(QDir(dir.path()).mkdir(QStringLiteral("sub.txt")));
        QVERIFY(!isOpenableReceivedFile(dir.filePath(QStringLiteral("sub.txt")), dir.path()));
        QVERIFY(!isOpenableReceivedFile(make(QDir(dir.path()), QStringLiteral("a.txt")), QString()));
    }

    void refusesASymlinkLeavingTheDownloadDir()
    {
        QTemporaryDir dir;
        QTemporaryDir other;
        const QString target = make(QDir(other.path()), QStringLiteral("a.txt"));
        QVERIFY(QFile::link(target, dir.filePath(QStringLiteral("link.txt"))));
        QVERIFY(!isOpenableReceivedFile(dir.filePath(QStringLiteral("link.txt")), dir.path()));
    }

    void refusesExecutableAndLauncherFiles()
    {
        QTemporaryDir dir;
        const QDir d(dir.path());
        QVERIFY(!isOpenableReceivedFile(make(d, QStringLiteral("tool"), QFileDevice::ReadOwner | QFileDevice::WriteOwner | QFileDevice::ExeOwner), dir.path()));
        for (const QString &name : {QStringLiteral("a.desktop"), QStringLiteral("b.AppImage"), QStringLiteral("c.sh"), QStringLiteral("d.RUN"), QStringLiteral("e.jar")}) {
            QVERIFY2(!isOpenableReceivedFile(make(d, name), dir.path()), qPrintable(name));
        }
    }
};

QTEST_APPLESS_MAIN(OpenableFileTest)
#include "openablefile_test.moc"
