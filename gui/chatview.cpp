#include "chatview.h"
#include <QScrollBar>
#include <QDesktopServices>
#include <QRegularExpression>
#include <QJsonDocument>
#include <QJsonArray>
#include <QUrl>

ChatView::ChatView(QWidget* parent) : QTextBrowser(parent) {
    setOpenLinks(false);
    setStyleSheet("QTextBrowser { background: #fff; color: #1e1e2e; border: none; padding: 8px; }");
    document()->setDefaultStyleSheet(
        "body { font-family: monospace; font-size: 10pt; }"
        "a { color: #a05000; }"
        "pre { background: #f5f5f5; padding: 8px; border-radius: 4px; white-space: pre-wrap; word-wrap: break-word; }"
        "code { background: #f0f0f0; padding: 1px 3px; border-radius: 2px; }"
        "table { border: 1px solid #ccc; margin: 6px 0; }"
        "th, td { border: 1px solid #ccc; padding: 4px 10px; }"
        "th { background: #f0f0f0; font-weight: bold; }"
    );
}

void ChatView::appendMessage(const QString& role, const QJsonValue& content) {
    if (role == "tool_request") {
        // Create a unified tool_block with result = null (not yet available)
        QJsonObject obj = content.toObject();
        QJsonObject msg;
        msg["role"] = "tool_block";
        msg["tool_call_id"] = obj["tool_call_id"];
        msg["name"] = obj["name"];
        msg["args"] = obj["args"];
        msg["result"] = QJsonValue::Null;
        msg["declined"] = false;
        m_messages.append(msg);
    } else if (role == "tool_result") {
        // Find matching tool_block and set result
        QJsonObject obj = content.toObject();
        QString tcId = obj["tool_call_id"].toString();
        for (int i = m_messages.size() - 1; i >= 0; --i) {
            QJsonObject msg = m_messages[i].toObject();
            if (msg["role"].toString() == "tool_block"
                && msg["tool_call_id"].toString() == tcId) {
                msg["result"] = obj["content"];
                msg["declined"] = obj["declined"].toBool(false);
                m_messages[i] = msg;
                break;
            }
        }
    } else {
        QJsonObject msg;
        msg["role"] = role;
        if (content.isString()) {
            msg["content"] = content.toString();
        } else if (content.isObject()) {
            QJsonObject obj = content.toObject();
            for (auto it = obj.begin(); it != obj.end(); ++it) {
                msg[it.key()] = it.value();
            }
        }
        m_messages.append(msg);
    }
    render();
}

void ChatView::clear() {
    m_messages = QJsonArray();
    m_expandedTools.clear();
    QTextBrowser::clear();
}

void ChatView::mousePressEvent(QMouseEvent* event) {
    if (event->button() == Qt::LeftButton) {
        QString anchor = anchorAt(event->pos());
        if (anchor.startsWith("toggle://")) {
            QString toolId = anchor.mid(9);  // strlen("toggle://")
            if (m_expandedTools.contains(toolId)) {
                m_expandedTools.remove(toolId);
            } else {
                m_expandedTools.insert(toolId);
            }
            render();
            return;
        }
        if (anchor.startsWith("http://") || anchor.startsWith("https://")) {
            QDesktopServices::openUrl(QUrl(anchor));
            return;
        }
    }
    QTextBrowser::mousePressEvent(event);
}

void ChatView::render() {
    auto* sb = verticalScrollBar();
    bool atBottom = sb->value() >= sb->maximum() - 30;
    int prev = sb->value();

    setHtml(buildHtml());

    if (atBottom) {
        sb->setValue(sb->maximum());
    } else {
        sb->setValue(prev);
    }
}

QString ChatView::buildHtml() {
    QString html = "<html><body>";
    for (const QJsonValue& v : m_messages) {
        html += renderMessage(v.toObject());
    }
    html += "</body></html>";
    return html;
}

QString ChatView::renderMessage(const QJsonObject& msg) const {
    QString role = msg["role"].toString();

    if (role == "user") {
        return QString(
            "<p style='color:#00008b;font-weight:bold;font-size:9pt;margin:8px 0 2px 0;'>"
            "&#x1F9D1; You</p>"
            "<p style='margin:2px 0 10px 0;white-space:pre-wrap;'>%1</p>"
        ).arg(escapeHtml(msg["content"].toString()));

    } else if (role == "assistant") {
        QString content = msg["content"].toString();
        if (content.isEmpty()) return "";
        return QString(
            "<p style='color:#006400;font-weight:bold;font-size:9pt;margin:8px 0 2px 0;'>"
            "&#x1F916; Assistant</p>"
            "<div style='margin:2px 0 10px 0;'>%1</div>"
        ).arg(markdownToHtml(content));

    } else if (role == "tool_block") {
        return renderToolBlock(msg);
    }

    return "";
}

QString ChatView::renderToolBlock(const QJsonObject& msg) const {
    QString toolCallId = msg["tool_call_id"].toString();
    QString name = msg["name"].toString();
    QJsonObject args = msg["args"].toObject();
    bool hasResult = !msg["result"].isNull() && !msg["result"].isUndefined();
    QString result = hasResult ? msg["result"].toString() : QString();
    bool declined = msg["declined"].toBool(false);
    bool expanded = m_expandedTools.contains(toolCallId);

    QString arrow = expanded ? "&#9660;" : "&#9654;";
    QString nameSafe = escapeHtml(name);

    // Build args preview
    QStringList argParts;
    for (auto it = args.begin(); it != args.end(); ++it) {
        QString val;
        if (it.value().isString()) {
            val = "'" + it.value().toString() + "'";
        } else {
            val = QJsonDocument(it.value().toObject()).toJson(QJsonDocument::Compact);
        }
        argParts.append(it.key() + "=" + val);
    }
    QString argsPreview = argParts.join(", ");
    bool truncated = argsPreview.length() > 60;
    if (truncated) {
        argsPreview = argsPreview.left(59);
    }

    QString label = arrow + "&nbsp;Tool:&nbsp;" + nameSafe;
    if (!argsPreview.isEmpty()) {
        label += "&nbsp;[" + escapeHtml(argsPreview) + "]";
        if (truncated) {
            label += "&#8230;";
        }
    }

    QString status;
    if (!hasResult && !declined) {
        status = "&nbsp;<i style='color:#888;'>(running&#8230;)</i>";
    } else if (declined) {
        status = "&nbsp;<i style='color:#cc0000;'>(declined)</i>";
    }

    QString header = QString(
        "<a href='toggle://%1' "
        "style='color:#a05000;text-decoration:none;font-weight:bold;'>%2</a>%3"
    ).arg(toolCallId, label, status);

    QString inner = "<div style='margin-bottom:2px;'>" + header + "</div>";

    if (expanded) {
        QString argsJson = escapeHtml(
            QJsonDocument(args).toJson(QJsonDocument::Indented));
        inner += QString(
            "<div style='margin-top:4px;'>"
            "<b>Arguments:</b>"
            "<pre style='background-color:#f0f0f0;padding:4px;margin:2px 0;font-size:9pt;'>%1</pre>"
            "</div>"
        ).arg(argsJson);

        if (hasResult) {
            QString resultLabel = declined ? "Result (declined)" : "Result";
            QString resultEscaped = escapeHtml(result);
            inner += QString(
                "<div>"
                "<b>%1:</b>"
                "<pre style='background-color:#f5f5f5;padding:4px;margin:2px 0;font-size:9pt;'>%2</pre>"
                "</div>"
            ).arg(resultLabel, resultEscaped);
        }
    }

    return QString(
        "<div style='border:1px solid #ddd;padding:4px 8px;margin:6px 0;background:#fafafa;'>"
        "%1"
        "</div>"
    ).arg(inner);
}

QString ChatView::markdownToHtml(const QString& md) const {
    // Escape HTML first — prevents literal < > " & in message
    // text from breaking QTextBrowser's HTML parser.  The Python
    // markdown library does this internally; our regex approach
    // must do it explicitly.  Backticks / asterisks / pipes are
    // untouched so the markdown regexes still match.
    QString result = md.toHtmlEscaped();

    // Convert ``` code blocks to pre/code FIRST — prevents pipe-delimited
    // lines inside code blocks from being falsely treated as tables.
    static QRegularExpression codeBlockRx("```(\\w*)\\n([\\s\\S]*?)```");
    result.replace(codeBlockRx, "<pre><code>\\2</code></pre>");

    // Convert markdown tables (now safe: code blocks already HTML-ified)
    result = convertMarkdownTables(result);

    // Convert inline code
    static QRegularExpression inlineCodeRx("`([^`]+)`");
    result.replace(inlineCodeRx, "<code>\\1</code>");

    // Convert **bold**
    static QRegularExpression boldRx("\\*\\*(.+?)\\*\\*");
    result.replace(boldRx, "<b>\\1</b>");

    // Convert *italic*  (must run after bold so **x** doesn't
    // get partially matched by the single-* pattern)
    static QRegularExpression italicRx("\\*(.+?)\\*");
    result.replace(italicRx, "<i>\\1</i>");

    // Qt doesn't support border-collapse; cellspacing="0" removes
    // inter-cell gaps so CSS borders read as a single collapsed border.
    result.replace("<table>", "<table cellspacing=\"0\">");

    // Convert double-newlines to paragraph breaks (matching Python
    // markdown library behaviour).  Blocks that already start with
    // a block-level HTML tag are left as-is; everything else gets
    // single-newlines → <br> and wrapped in <p>…</p>.
    result = paragraphize(result);

    return result;
}

QString ChatView::paragraphize(const QString& html) const {
    QStringList parts = html.split("\n\n");
    for (int i = 0; i < parts.size(); ++i) {
        QString p = parts[i].trimmed();
        if (p.isEmpty()) continue;
        bool isBlock = p.startsWith("<table") || p.startsWith("<pre")
                    || p.startsWith("<div")  || p.startsWith("<p")
                    || p.startsWith("<h1")  || p.startsWith("<h2")
                    || p.startsWith("<h3")  || p.startsWith("<h4")
                    || p.startsWith("<ul")  || p.startsWith("<ol")
                    || p.startsWith("<li")  || p.startsWith("<blockquote");
        if (!isBlock) {
            p.replace("\n", "<br>");
            parts[i] = "<p>" + p + "</p>";
        } else {
            parts[i] = p;
        }
    }
    return parts.join("\n");
}

QString ChatView::convertMarkdownTables(const QString& md) const {
    // Convert markdown tables to HTML.
    // Format: header row, separator row (e.g. |---|---|), data rows.
    QStringList lines = md.split('\n');
    QString out;
    QStringList tableBuf;
    bool inTable = false;

    for (int i = 0; i < lines.size(); ++i) {
        QString line = lines[i].trimmed();
        bool isTableLine = line.startsWith('|') && line.endsWith('|');

        if (isTableLine) {
            tableBuf.append(line);
            inTable = true;
        } else {
            if (inTable) {
                // Render accumulated table
                if (tableBuf.size() >= 2) {
                    // Check that second line is a separator (e.g. |---|:---:|---|)
                    QString sep = tableBuf[1];
                    static QRegularExpression sepRx("^\\|[\\s:\\-|]+\\|$");
                    if (sepRx.match(sep).hasMatch()) {
                        // Valid table — render to HTML
                        out += "<table>\n";

                        // Header
                        QStringList headerCells = tableBuf[0].mid(1, tableBuf[0].length() - 2).split('|');
                        out += "<tr>";
                        for (const QString& cell : headerCells) {
                            out += "<th>" + cell.trimmed() + "</th>";
                        }
                        out += "</tr>\n";

                        // Data rows (skip header[0] and separator[1])
                        for (int r = 2; r < tableBuf.size(); ++r) {
                            QStringList cells = tableBuf[r].mid(1, tableBuf[r].length() - 2).split('|');
                            out += "<tr>";
                            for (const QString& cell : cells) {
                                out += "<td>" + cell.trimmed() + "</td>";
                            }
                            out += "</tr>\n";
                        }
                        out += "</table>\n";
                    } else {
                        // Not a valid table — output raw lines
                        for (const QString& l : tableBuf) {
                            out += l + "\n";
                        }
                    }
                } else {
                    // Single-line table row, not enough for a table
                    for (const QString& l : tableBuf) {
                        out += l + "\n";
                    }
                }
                tableBuf.clear();
                inTable = false;
            }
            out += lines[i] + "\n";
        }
    }

    // Handle trailing table at EOF
    if (inTable && tableBuf.size() >= 2) {
        QString sep = tableBuf[1];
        static QRegularExpression sepRx2("^\\|[\\s:\\-|]+\\|$");
        if (sepRx2.match(sep).hasMatch()) {
            out += "<table>\n";
            QStringList headerCells = tableBuf[0].mid(1, tableBuf[0].length() - 2).split('|');
            out += "<tr>";
            for (const QString& cell : headerCells) {
                out += "<th>" + cell.trimmed() + "</th>";
            }
            out += "</tr>\n";
            for (int r = 2; r < tableBuf.size(); ++r) {
                QStringList cells = tableBuf[r].mid(1, tableBuf[r].length() - 2).split('|');
                out += "<tr>";
                for (const QString& cell : cells) {
                    out += "<td>" + cell.trimmed() + "</td>";
                }
                out += "</tr>\n";
            }
            out += "</table>\n";
        } else {
            for (const QString& l : tableBuf) {
                out += l + "\n";
            }
        }
    } else if (inTable) {
        for (const QString& l : tableBuf) {
            out += l + "\n";
        }
    }

    return out;
}

QString ChatView::escapeHtml(const QString& text) const {
    return text.toHtmlEscaped();
}

QVariant ChatView::loadResource(int type, const QUrl& url) {
    if (url.scheme().startsWith("http")) {
        QDesktopServices::openUrl(url);
        return QVariant();
    }
    return QTextBrowser::loadResource(type, url);
}
