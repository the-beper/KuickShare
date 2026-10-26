#pragma once

#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QString>

/// True when `path` may be offered for "Open": files come from an untrusted peer, so only a regular
/// file whose real path is inside `downloadDir` qualifies, and never one that could run code
/// (executable bit, or a launcher-like extension).
inline bool isOpenableReceivedFile(const QString &path, const QString &downloadDir)
{
    const QFileInfo info(path);
    const QString real = info.canonicalFilePath(); // empty when the file does not exist
    const QString root = QDir(downloadDir).canonicalPath();
    if (real.isEmpty() || root.isEmpty() || !QFileInfo(real).isFile() || !real.startsWith(root + QLatin1Char('/'))) {
        return false;
    }
    if (QFile::permissions(real) & (QFileDevice::ExeOwner | QFileDevice::ExeGroup | QFileDevice::ExeOther)) {
        return false;
    }
    for (const QLatin1String suffix : {QLatin1String("desktop"), QLatin1String("appimage"), QLatin1String("sh"), QLatin1String("run"), QLatin1String("jar")}) {
        if (QFileInfo(real).suffix().compare(suffix, Qt::CaseInsensitive) == 0) {
            return false;
        }
    }
    return true;
}
