#include "chathistory.h"
#include <QVBoxLayout>
#include <QHBoxLayout>
#include <QFrame>
#include <QFont>

ChatHistoryWidget::ChatHistoryWidget(QWidget* parent) : QWidget(parent) {
    setupUi();
}

void ChatHistoryWidget::setupUi() {
    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(8, 8, 8, 8);
    layout->setSpacing(4);

    m_newChatBtn = new QPushButton("+ New Chat");
    m_newChatBtn->setFixedHeight(36);
    connect(m_newChatBtn, &QPushButton::clicked, this, &ChatHistoryWidget::newChatRequested);
    layout->addWidget(m_newChatBtn);

    m_settingsBtn = new QPushButton("⚙ Settings");
    m_settingsBtn->setFixedHeight(36);
    connect(m_settingsBtn, &QPushButton::clicked, this, &ChatHistoryWidget::settingsRequested);
    layout->addWidget(m_settingsBtn);

    layout->addSpacing(8);

    auto* divider = new QFrame;
    divider->setFrameShape(QFrame::HLine);
    divider->setFrameShadow(QFrame::Sunken);
    layout->addWidget(divider);

    layout->addSpacing(4);

    m_chatList = new QListWidget;
    m_chatList->setStyleSheet("QListWidget::item { padding: 4px; }");
    connect(m_chatList, &QListWidget::itemClicked, this, &ChatHistoryWidget::onItemClicked);
    layout->addWidget(m_chatList, 1);

    layout->addSpacing(8);

    // Quick Settings panel
    auto* qsFrame = new QFrame;
    qsFrame->setFrameShape(QFrame::StyledPanel);
    auto* qsLayout = new QVBoxLayout(qsFrame);
    qsLayout->setContentsMargins(8, 8, 8, 8);
    qsLayout->setSpacing(4);

    auto* statusRow = new QHBoxLayout;
    auto* statusLabel = new QLabel("Status");
    statusLabel->setStyleSheet("font-weight: bold; color: #000;");
    statusRow->addWidget(statusLabel);

    m_statusDot = new QLabel("●");
    m_statusDot->setStyleSheet("color: #a6e3a1; font-size: 14px;");
    statusRow->addWidget(m_statusDot);

    m_statusText = new QLabel("Idle");
    m_statusText->setStyleSheet("color: #000;");
    statusRow->addWidget(m_statusText);
    statusRow->addStretch();
    qsLayout->addLayout(statusRow);

    auto* qsDivider = new QFrame;
    qsDivider->setFrameShape(QFrame::HLine);
    qsLayout->addWidget(qsDivider);

    m_modelLabel = new QLabel("Model: gpt-4o");
    m_modelLabel->setStyleSheet("color: #000;");
    qsLayout->addWidget(m_modelLabel);

    m_confirmLabel = new QLabel("Tool Confirm: None");
    m_confirmLabel->setStyleSheet("color: #000;");
    qsLayout->addWidget(m_confirmLabel);

    m_tokensLabel = new QLabel("Tokens: —");
    m_tokensLabel->setStyleSheet("color: #000;");
    qsLayout->addWidget(m_tokensLabel);

    layout->addWidget(qsFrame);

    m_blinkTimer = new QTimer(this);
    m_blinkTimer->setInterval(500);
    connect(m_blinkTimer, &QTimer::timeout, this, &ChatHistoryWidget::blinkDot);
}

void ChatHistoryWidget::loadChats(const QJsonArray& chats) {
    m_chatList->clear();
    for (const QJsonValue& v : chats) {
        QJsonObject chat = v.toObject();
        auto* item = new QListWidgetItem(chat["title"].toString());
        item->setData(Qt::UserRole, chat["id"].toString());
        m_chatList->addItem(item);
    }
}

void ChatHistoryWidget::selectChatById(const QString& id) {
    for (int i = 0; i < m_chatList->count(); i++) {
        if (m_chatList->item(i)->data(Qt::UserRole).toString() == id) {
            m_chatList->setCurrentRow(i);
            return;
        }
    }
}

void ChatHistoryWidget::updateChatTitle(const QString& id, const QString& title) {
    for (int i = 0; i < m_chatList->count(); i++) {
        if (m_chatList->item(i)->data(Qt::UserRole).toString() == id) {
            m_chatList->item(i)->setText(title);
            return;
        }
    }
}

void ChatHistoryWidget::onItemClicked(QListWidgetItem* item) {
    emit chatSelected(item->data(Qt::UserRole).toString());
}

void ChatHistoryWidget::setThinking(bool thinking) {
    if (thinking) {
        m_dotPhase = true;
        m_statusDot->setStyleSheet("color: #f38ba8; font-size: 14px;");
        m_statusText->setText("Thinking…");
        m_blinkTimer->start();
    } else {
        m_blinkTimer->stop();
        m_statusDot->setStyleSheet("color: #a6e3a1; font-size: 14px;");
        m_statusText->setText("Idle");
    }
}

void ChatHistoryWidget::setToolRunning(bool running) {
    if (running) {
        m_blinkTimer->stop();
        m_statusDot->setStyleSheet("color: #fab387; font-size: 14px;");
        m_statusText->setText("Tool running");
    } else {
        m_dotPhase = true;
        m_statusDot->setStyleSheet("color: #f38ba8; font-size: 14px;");
        m_statusText->setText("Thinking…");
        m_blinkTimer->start();
    }
}

void ChatHistoryWidget::blinkDot() {
    m_dotPhase = !m_dotPhase;
    QString color = m_dotPhase ? "#f38ba8" : "transparent";
    m_statusDot->setStyleSheet(QString("color: %1; font-size: 14px;").arg(color));
}

void ChatHistoryWidget::updateQuickSettings(const QString& model, const QString& confirm) {
    m_modelLabel->setText("Model: " + model);
    QString label;
    if (confirm == "all") label = "Tool Confirm: YOLO";
    else if (confirm == "safe") label = "Tool Confirm: Safe";
    else label = "Tool Confirm: None";
    m_confirmLabel->setText(label);
}

void ChatHistoryWidget::updateTokenUsage(int prompt, int completion) {
    if (prompt || completion) {
        m_tokensLabel->setText(QString("Tokens: %1 in / %2 out").arg(prompt).arg(completion));
    } else {
        m_tokensLabel->setText("Tokens: —");
    }
}
