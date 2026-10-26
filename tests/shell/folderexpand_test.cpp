#include <QtTest>

#include "folderexpand.h"

class FolderExpandTest : public QObject
{
    Q_OBJECT

private:
    static QString make(const QDir &dir, const QString &name)
    {
        QDir().mkpath(QFileInfo(dir.filePath(name)).absolutePath());
        QFile file(dir.filePath(name));
        if (file.open(QIODevice::WriteOnly)) {
            file.write("x");
        }
        return dir.filePath(name);
    }

private Q_SLOTS:
    void aPlainFilePassesThrough()
    {
        QTemporaryDir dir;
        const QString file = make(QDir(dir.path()), QStringLiteral("a.txt"));
        const SendExpansion r = expandForSend({file, dir.filePath(QStringLiteral("missing"))});
        QCOMPARE(r.files, QStringList{file});
        QCOMPARE(r.skipped, QStringList{dir.filePath(QStringLiteral("missing"))});
    }

    void flattensNestedFoldersSorted()
    {
        QTemporaryDir dir;
        const QDir d(dir.path());
        const QString c = make(d, QStringLiteral("top/c.txt"));
        const QString b = make(d, QStringLiteral("top/sub/deeper/b.txt"));
        const QString a = make(d, QStringLiteral("top/sub/a.txt"));
        const QString z = make(d, QStringLiteral("top/z.txt"));
        const SendExpansion r = expandForSend({dir.filePath(QStringLiteral("top"))});
        // Plain path order: top/c.txt < top/sub/a.txt < top/sub/deeper/b.txt < top/z.txt.
        QCOMPARE(r.files, (QStringList{c, a, b, z}));
        QVERIFY(r.tooMany.isEmpty() && r.empty.isEmpty());
    }

    void skipsHiddenFilesAndFolders()
    {
        QTemporaryDir dir;
        const QDir d(dir.path());
        const QString a = make(d, QStringLiteral("top/a.txt"));
        make(d, QStringLiteral("top/.hidden"));
        make(d, QStringLiteral("top/.git/config"));
        make(d, QStringLiteral("top/sub/.secret/x.txt"));
        QCOMPARE(expandForSend({dir.filePath(QStringLiteral("top"))}).files, QStringList{a});
    }

    void skipsSymlinkFilesAndDirectories()
    {
        QTemporaryDir dir;
        QTemporaryDir other;
        const QDir d(dir.path());
        const QString a = make(d, QStringLiteral("top/a.txt"));
        const QString outside = make(QDir(other.path()), QStringLiteral("o.txt"));
        QVERIFY(QFile::link(outside, d.filePath(QStringLiteral("top/link.txt"))));
        QVERIFY(QFile::link(other.path(), d.filePath(QStringLiteral("top/linkdir"))));
        QVERIFY(QFile::link(d.filePath(QStringLiteral("top")), d.filePath(QStringLiteral("top/loop"))));
        QCOMPARE(expandForSend({dir.filePath(QStringLiteral("top"))}).files, QStringList{a});
    }

    void overTheLimitAddsNothing()
    {
        QTemporaryDir dir;
        const QDir d(dir.path());
        for (int i = 0; i < 4; ++i) {
            make(d, QStringLiteral("big/sub/f%1.txt").arg(i));
        }
        const QString ok = make(d, QStringLiteral("ok/a.txt"));
        const SendExpansion r = expandForSend({dir.filePath(QStringLiteral("big")), dir.filePath(QStringLiteral("ok"))}, 3);
        QCOMPARE(r.tooMany, QStringList{dir.filePath(QStringLiteral("big"))});
        QCOMPARE(r.files, QStringList{ok});
        // Exactly the limit is fine.
        QCOMPARE(expandForSend({dir.filePath(QStringLiteral("big"))}, 4).files.size(), 4);
    }

    void theTotalLimitCoversSeveralFolders()
    {
        QTemporaryDir dir;
        const QDir d(dir.path());
        const QString plain = make(d, QStringLiteral("p.txt"));
        make(d, QStringLiteral("one/a.txt"));
        make(d, QStringLiteral("one/b.txt"));
        make(d, QStringLiteral("two/c.txt"));
        make(d, QStringLiteral("two/d.txt"));
        const SendExpansion r = expandForSend({plain, dir.filePath(QStringLiteral("one")), dir.filePath(QStringLiteral("two"))}, 4);
        QCOMPARE(r.files.size(), 3);
        QCOMPARE(r.tooMany, QStringList{dir.filePath(QStringLiteral("two"))});
    }

    void theEntryBoundStopsHugeTrees()
    {
        QTemporaryDir dir;
        const QDir d(dir.path());
        for (int i = 0; i < 10; ++i) {
            QVERIFY(d.mkpath(QStringLiteral("tree/d%1").arg(i)));
        }
        QVERIFY(expandForSend({dir.filePath(QStringLiteral("tree"))}, 500, 5).tooMany.size() == 1);
        // Within the bound the folder is only empty.
        QCOMPARE(expandForSend({dir.filePath(QStringLiteral("tree"))}, 500, 10).empty.size(), 1);
    }

    void anEmptyFolderIsReported()
    {
        QTemporaryDir dir;
        QVERIFY(QDir(dir.path()).mkdir(QStringLiteral("e")));
        make(QDir(dir.path()), QStringLiteral("e2/.hidden"));
        const SendExpansion r = expandForSend({dir.filePath(QStringLiteral("e")), dir.filePath(QStringLiteral("e2"))});
        QVERIFY(r.files.isEmpty());
        QCOMPARE(r.empty.size(), 2);
    }
};

QTEST_APPLESS_MAIN(FolderExpandTest)
#include "folderexpand_test.moc"
