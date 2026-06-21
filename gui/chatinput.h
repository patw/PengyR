#pragma once
#include <QWidget>
#include <QTextEdit>
#include <QPushButton>
#include <QStringList>

class ChatInputWidget : public QWidget {
    Q_OBJECT
public:
    explicit ChatInputWidget(QWidget* parent = nullptr);

signals:
    void messageSent(const QString& text, const QStringList& images);

private slots:
    void onSubmit();
    void pickFile();

private:
    bool eventFilter(QObject* obj, QEvent* event) override;
    QTextEdit* m_edit;
    QPushButton* m_attachBtn;
};
