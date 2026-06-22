#include <QApplication>
#include <QString>
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
    requireContains(code, "<pre><code>fn main() {}", "code block content");

    QString table = view.testMarkdownToHtml("| A | B |\n|---|---|\n| 1 | 2 |");
    requireContains(table, "<table", "table renders");
    requireContains(table, "<th>A</th>", "table header");
    requireContains(table, "<td>1</td>", "table cell");

    return 0;
}
