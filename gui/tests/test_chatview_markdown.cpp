#include <QApplication>
#include <QString>
#include <QJsonObject>
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

    return 0;
}
