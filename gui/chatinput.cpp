#include "chatinput.h"
#include <QHBoxLayout>
#include <QVBoxLayout>
#include <QFileDialog>
#include <QKeyEvent>
#include <QMessageBox>
#include <QFontDatabase>

ChatInputWidget::ChatInputWidget(QWidget* parent) : QWidget(parent) {
    auto* row = new QHBoxLayout(this);
    row->setContentsMargins(0, 0, 0, 0);

    m_attachBtn = new QPushButton("📎");
    m_attachBtn->setFixedSize(32, 32);
    m_attachBtn->setStyleSheet(
        "QPushButton { background: transparent; border: 1px solid #ccc; border-radius: 6px; }"
        "QPushButton:hover { background: #f0f0f0; }");
    connect(m_attachBtn, &QPushButton::clicked, this, &ChatInputWidget::pickFile);
    row->addWidget(m_attachBtn);

    m_edit = new QTextEdit;
    auto font = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    font.setPointSize(10);
    m_edit->setFont(font);
    m_edit->setPlaceholderText("Type a message... (Enter to send, Shift+Enter for new line)");
    m_edit->setMaximumHeight(60);
    m_edit->setMinimumHeight(40);
    m_edit->setStyleSheet(
        "QTextEdit { background: #fff; color: #1e1e2e; border: 1px solid #ccc; "
        "border-radius: 8px; padding: 6px 10px; }"
        "QTextEdit:focus { border-color: #89b4fa; }");
    m_edit->installEventFilter(this);
    row->addWidget(m_edit);
}

void ChatInputWidget::onSubmit() {
    QString text = m_edit->toPlainText().trimmed();
    if (text.isEmpty()) return;
    m_edit->clear();
    emit messageSent(text, {});
}

void ChatInputWidget::pickFile() {
    QString path = QFileDialog::getOpenFileName(this, "Attach File");
    if (!path.isEmpty()) {
        // Simple file attachment — just append the filename
        m_edit->insertPlainText(QString("[File: %1]\n").arg(path.section('/', -1)));
    }
}

bool ChatInputWidget::eventFilter(QObject* obj, QEvent* event) {
    if (obj == m_edit && event->type() == QEvent::KeyPress) {
        auto* ke = static_cast<QKeyEvent*>(event);
        if (ke->key() == Qt::Key_Return && !(ke->modifiers() & Qt::ShiftModifier)) {
            onSubmit();
            return true;
        }
    }
    return QWidget::eventFilter(obj, event);
}
