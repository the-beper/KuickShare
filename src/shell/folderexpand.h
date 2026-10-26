#pragma once

#include <QDir>
#include <QDirIterator>
#include <QFileInfo>
#include <QString>
#include <QStringList>

/// Most files one folder may add to the queue.
inline constexpr int maxFolderFiles = 500;
/// Most directory entries visited in one folder, so a huge tree cannot block the UI.
inline constexpr int maxFolderEntries = 20000;

/// What `expandForSend` made of a list of paths.
struct SendExpansion
{
    QStringList files;   ///< regular files to queue, in the order given; the files of a folder are sorted
    QStringList tooMany; ///< folders with more than the limit: nothing of them is in `files`
    QStringList empty;   ///< folders without any file to send
    QStringList skipped; ///< neither a file nor a folder (missing, ...)
};

/// Folders are flattened: every regular file below them is queued as if chosen on its own, the structure is
/// not kept. Hidden files and folders and symbolic links are skipped (so no loop, and nothing outside the
/// folder); a path given directly may be a link. A folder that would bring the call past `limit` files in
/// total, or that has more than `maxEntries` entries, adds none of its files and is listed in `tooMany`.
inline SendExpansion expandForSend(const QStringList &paths, int limit = maxFolderFiles, int maxEntries = maxFolderEntries)
{
    SendExpansion out;
    for (const QString &path : paths) {
        const QFileInfo info(path);
        if (info.isFile()) {
            out.files << path;
            continue;
        }
        if (!info.isDir()) {
            out.skipped << path;
            continue;
        }
        QStringList found;
        bool over = false;
        int entries = 0;
        const int room = limit - int(out.files.size());
        QDirIterator it(path, QDir::Files | QDir::AllDirs | QDir::NoDotAndDotDot | QDir::NoSymLinks, QDirIterator::Subdirectories);
        while (it.hasNext()) {
            it.next();
            if (++entries > maxEntries) {
                over = true;
                break;
            }
            if (!it.fileInfo().isFile()) {
                continue;
            }
            if (found.size() >= room) {
                over = true;
                break;
            }
            found << it.filePath();
        }
        if (over) {
            out.tooMany << path;
        } else if (found.isEmpty()) {
            out.empty << path;
        } else {
            found.sort();
            out.files << found;
        }
    }
    return out;
}
