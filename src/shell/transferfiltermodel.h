#pragma once

#include <QSortFilterProxyModel>

/// The transfer list as the Transfers page shows it: a TransferModel narrowed by direction, status
/// group and a search text. The filters are not saved.
class TransferFilterModel : public QSortFilterProxyModel
{
    Q_OBJECT
    /// "all", "inbound" or "outbound".
    Q_PROPERTY(QString direction READ direction WRITE setDirection NOTIFY directionChanged)
    /// "all", "active" (not finished), "done" (Finished) or "failed" (any other final state).
    Q_PROPERTY(QString status READ status WRITE setStatus NOTIFY statusChanged)
    /// Matches the device name, the received text and the file names, ignoring case.
    Q_PROPERTY(QString text READ text WRITE setText NOTIFY textChanged)
    /// Some filter is set.
    Q_PROPERTY(bool filtered READ filtered NOTIFY filteredChanged)
    /// Rows of the unfiltered model.
    Q_PROPERTY(int sourceCount READ sourceCount NOTIFY sourceCountChanged)

public:
    explicit TransferFilterModel(QAbstractItemModel *source, QObject *parent = nullptr);

    QString direction() const { return m_direction; }
    QString status() const { return m_status; }
    QString text() const { return m_text; }
    bool filtered() const { return m_direction != QLatin1String("all") || m_status != QLatin1String("all") || !m_text.isEmpty(); }
    int sourceCount() const { return sourceModel()->rowCount(); }

    void setDirection(const QString &direction);
    void setStatus(const QString &status);
    void setText(const QString &text);
    Q_INVOKABLE void clearFilters();
    /// Row of the unfiltered model for a row of this one.
    Q_INVOKABLE int sourceRow(int row) const { return mapToSource(index(row, 0)).row(); }

signals:
    void directionChanged();
    void statusChanged();
    void textChanged();
    void filteredChanged();
    void sourceCountChanged();

protected:
    bool filterAcceptsRow(int sourceRow, const QModelIndex &sourceParent) const override;

private:
    void change(QString &field, const QString &value, void (TransferFilterModel::*changed)());

    QString m_direction = QStringLiteral("all");
    QString m_status = QStringLiteral("all");
    QString m_text;
};
