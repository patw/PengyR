#pragma once
#include <QTextBrowser>
#include <QJsonObject>
#include <QJsonArray>
#include <QJsonValue>
#include <QSet>
#include <QMouseEvent>

class ChatView : public QTextBrowser {
    Q_OBJECT
public:
    explicit ChatView(QWidget* parent = nullptr);
    void appendMessage(const QString& role, const QJsonValue& content);
    void appendMessageText(const QString& role, const QString& text) {
        appendMessage(role, QJsonValue(text));
    }
    void clear();

protected:
    void mousePressEvent(QMouseEvent* event) override;
    QVariant loadResource(int type, const QUrl& url) override;

private:
    void render();
    QString buildHtml();
    QString renderMessage(const QJsonObject& msg) const;
    QString renderToolBlock(const QJsonObject& msg) const;
    QString markdownToHtml(const QString& md) const;
    QString convertMarkdownTables(const QString& md) const;
    QString paragraphize(const QString& html) const;
    QString escapeHtml(const QString& text) const;

    QJsonArray m_messages;
    QSet<QString> m_expandedTools;
};
