#include <QApplication>
#include <QString>
#include <QJsonObject>
#include <QScrollBar>
#include <iostream>
#include "../chatview.h"

static void requireContains(const QString& haystack, const QString& needle, const char* label) {
    if (!haystack.contains(needle)) {
        std::cerr << "FAIL: " << label << "\nExpected to contain: "
                  << needle.toStdString() << "\nGot:\n"
                  << haystack.toStdString() << std::endl;
        std::exit(1);
    }
}

static void requireNotContains(const QString& haystack, const QString& needle, const char* label) {
    if (haystack.contains(needle)) {
        std::cerr << "FAIL: " << label << "\nExpected NOT to contain: "
                  << needle.toStdString() << "\nGot:\n"
                  << haystack.toStdString() << std::endl;
        std::exit(1);
    }
}

static void requireEqual(const QString& got, const QString& want, const char* label) {
    if (got != want) {
        std::cerr << "FAIL: " << label << "\nExpected:\n"
                  << want.toStdString() << "\nGot:\n"
                  << got.toStdString() << std::endl;
        std::exit(1);
    }
}

int main(int argc, char** argv) {
    QApplication app(argc, argv);
    ChatView view;

    QString unordered = view.testMarkdownToHtml("- one\n- two");
    requireContains(unordered, "<ul>", "unordered list opens");
    requireContains(unordered, "<li>one</li>", "unordered list item one");
    requireContains(unordered, "<li>two</li>", "unordered list item two");
    requireContains(unordered, "</ul>", "unordered list closes");

    QString ordered = view.testMarkdownToHtml("1. first\n2. second");
    requireContains(ordered, "<ol>", "ordered list opens");
    requireContains(ordered, "<li>first</li>", "ordered list item first");
    requireContains(ordered, "<li>second</li>", "ordered list item second");
    requireContains(ordered, "</ol>", "ordered list closes");

    QString quote = view.testMarkdownToHtml("> quoted\n> continued");
    requireContains(quote, "<blockquote>", "blockquote opens");
    requireContains(quote, "quoted", "blockquote content");
    requireContains(quote, "continued", "blockquote continuation");
    requireContains(quote, "</blockquote>", "blockquote closes");

    QString hr = view.testMarkdownToHtml("before\n\n---\n\nafter");
    requireContains(hr, "<hr>", "horizontal rule");

    QString code = view.testMarkdownToHtml("```rust\nfn main() {}\n```");
    requireContains(code, "class='code-lang'>rust</div>", "code language label");
    requireContains(code, "<pre><code>", "code block opens");
    requireContains(code, "main() {}", "code block content");
    requireContains(code, "<span style='color:", "code block keyword is highlighted");
    requireContains(code, ">fn</span>", "rust keyword fn is highlighted");

    QString pyCode = view.testMarkdownToHtml("```python\n# a comment\nx = 42\ns = \"hi\"\n```");
    requireContains(pyCode, ">42</span>", "python number is highlighted");
    requireContains(pyCode, "# a comment</span>", "python comment is highlighted");
    requireContains(pyCode, "&quot;hi&quot;</span>", "python string is highlighted");

    QString plainCode = view.testMarkdownToHtml("```\nplain text, no language\n```");
    requireContains(plainCode, "<pre><code>plain text, no language", "unlabeled code block is left unhighlighted");

    QString table = view.testMarkdownToHtml("| A | B |\n|---|---|\n| 1 | 2 |");
    requireContains(table, "<table", "table renders");
    requireContains(table, "<th>A</th>", "table header");
    requireContains(table, "<td>1</td>", "table cell");

    // ── render cache ────────────────────────────────────────────────────
    // buildHtml() memoises per-message HTML. Every path that changes a
    // message's rendering must invalidate its entry, so a cached render must
    // always equal a cold one.
    {
        ChatView v;

        v.appendMessageText("user", "hello **world**", false);
        QJsonObject asst;
        asst["content"] = QString("```python\nprint(1)\n```");
        asst["reasoning_content"] = QString("head\nTAIL-LINE");
        v.appendMessage("assistant", asst, false);
        requireEqual(v.testBuildHtml(), v.testBuildHtmlCold(), "cached == cold (user+assistant)");
        requireEqual(QString::number(v.testCacheSize()), "2", "cache tracks appends");

        QJsonObject req;
        req["tool_call_id"] = "t1";
        req["name"] = "read_file";
        req["args"] = QJsonObject{{"path", "/x"}};
        v.appendMessage("tool_request", req, false);
        requireContains(v.testBuildHtml(), "running", "pending tool shows (running...)");

        QJsonObject res;
        res["tool_call_id"] = "t1";
        res["content"] = QString("SECRET-PAYLOAD");
        res["declined"] = false;
        v.appendMessage("tool_result", res, false);
        QString afterResult = v.testBuildHtml();
        requireNotContains(afterResult, "running", "tool result clears (running...)");
        requireEqual(afterResult, v.testBuildHtmlCold(), "cached == cold (after tool result)");

        v.testBuildHtml();  // warm while collapsed
        v.testExpandTool("t1");
        requireContains(v.testBuildHtml(), "SECRET-PAYLOAD", "tool expand shows result");
        requireEqual(v.testBuildHtml(), v.testBuildHtmlCold(), "cached == cold (tool expanded)");

        v.testBuildHtml();  // warm while collapsed
        v.testExpandReasoning(1);
        requireContains(v.testBuildHtml(), "TAIL-LINE", "reasoning expand shows full text");

        // A theme swap rebuilds the highlighter colours, so cached code blocks
        // are stale even though no message content changed.
        v.testBuildHtml();
        v.applyTheme(makeTheme("dark", "default"), 100);
        requireEqual(v.testBuildHtml(), v.testBuildHtmlCold(), "cached == cold (after theme change)");

        v.clear();
        requireEqual(QString::number(v.testCacheSize()), "0", "clear resets cache");
    }

    // ── auto-scroll pin (regression: "snaps back up to old history") ────
    // setHtml() replaces the whole document and resets the scrollbar to 0.
    // The old render() decided "am I at the bottom?" by reading sb->value()
    // *after* that reset — so any render landing while a previous render's
    // deferred scroll-to-bottom was still pending read value()==0, concluded
    // the user had scrolled up, and pinned the view to the top of the history.
    // These guard the explicit m_autoScroll flag that replaced that check.
    {
        ChatView v;
        v.resize(400, 600);
        v.show();
        app.processEvents();

        // Fill with enough content to make the document taller than the viewport.
        for (int i = 0; i < 60; ++i)
            v.appendMessageText("assistant", QString("line %1 ").arg(i).repeated(20), false);
        v.renderNow();
        app.processEvents();

        requireEqual(v.testAutoScroll() ? "true" : "false", "true", "pin starts true");
        v.renderNow();
        app.processEvents();
        requireEqual(v.testAutoScroll() ? "true" : "false", "true", "pin survives a render");

        // Two renders back-to-back before the event loop flushes the deferred
        // scroll — the exact sequence that used to read value()==0 and snap
        // the view to the top.
        v.renderNow();
        v.renderNow();
        app.processEvents();
        requireEqual(v.testAutoScroll() ? "true" : "false", "true", "interleaved render keeps the pin");

        // Genuine scroll up clears the pin
        v.verticalScrollBar()->setValue(0);
        app.processEvents();
        requireEqual(v.testAutoScroll() ? "true" : "false", "false", "genuine scroll up clears the pin");

        // A render while cleared must NOT yank to the bottom
        v.renderNow();
        app.processEvents();
        auto* sb = v.verticalScrollBar();
        if (sb->value() >= sb->maximum() / 2) {
            std::cerr << "FAIL: cleared pin yanked to bottom\n"
                      << "value=" << sb->value() << " max=" << sb->maximum() << std::endl;
            std::exit(1);
        }

        // clear() resets the pin
        v.clear();
        requireEqual(v.testAutoScroll() ? "true" : "false", "true", "clear resets the pin");
    }

    return 0;
}
