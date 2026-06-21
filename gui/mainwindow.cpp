#include "mainwindow.h"
#include "chathistory.h"
#include "chatview.h"
#include "chatinput.h"
#include "chatworker.h"
#include "settingsdialog.h"
#include "pengy_ffi.h"

#include <QSplitter>
#include <QVBoxLayout>
#include <QHBoxLayout>
#include <QPushButton>
#include <QJsonDocument>
#include <QJsonArray>
#include <QJsonObject>
#include <QMessageBox>
#include <QFile>
#include <QMimeDatabase>
#include <QMimeType>

MainWindow::MainWindow(QWidget* parent) : QMainWindow(parent) {
    // Load config
    char* cfgJson = pengy_config_load();
    QJsonDocument cfgDoc = QJsonDocument::fromJson(QByteArray(cfgJson));
    m_config = cfgDoc.object();
    pengy_free(cfgJson);

    setupUi();
    updateLlmClient();
    loadChatList();

    // Show actual config in the status bar
    m_chatHistory->updateQuickSettings(
        m_config["model"].toString("gpt-4o"),
        m_config["tool_confirmation"].toString("none"));

    // Poll for tool confirmation requests from the worker thread
    m_confirmTimer = new QTimer(this);
    m_confirmTimer->setInterval(100);
    connect(m_confirmTimer, &QTimer::timeout, this, &MainWindow::pollToolConfirmation);

    // Create initial chat if none
    if (m_chats.isEmpty()) {
        createNewChat();
    } else {
        loadChat(m_chats[0].toObject()["id"].toString());
    }
}

void MainWindow::setupUi() {
    setWindowTitle("Pengy 🐧");
    resize(1100, 700);

    auto* central = new QWidget;
    setCentralWidget(central);
    auto* mainLayout = new QHBoxLayout(central);
    mainLayout->setSpacing(0);
    mainLayout->setContentsMargins(0, 0, 0, 0);

    // Left sidebar
    auto* leftSplitter = new QSplitter(Qt::Vertical);
    m_chatHistory = new ChatHistoryWidget;
    connect(m_chatHistory, &ChatHistoryWidget::chatSelected, this, &MainWindow::loadChat);
    connect(m_chatHistory, &ChatHistoryWidget::newChatRequested, this, &MainWindow::createNewChat);
    connect(m_chatHistory, &ChatHistoryWidget::settingsRequested, this, &MainWindow::openSettings);
    connect(m_chatHistory, &ChatHistoryWidget::deleteRequested, this, &MainWindow::deleteChat);
    leftSplitter->addWidget(m_chatHistory);

    // Right pane
    auto* rightSplitter = new QSplitter(Qt::Vertical);
    m_chatView = new ChatView;
    rightSplitter->addWidget(m_chatView);

    // Input row
    auto* inputRow = new QWidget;
    auto* inputLayout = new QHBoxLayout(inputRow);
    inputLayout->setContentsMargins(8, 4, 8, 4);
    m_chatInput = new ChatInputWidget;
    connect(m_chatInput, &ChatInputWidget::messageSent, this, &MainWindow::sendMessage);
    inputLayout->addWidget(m_chatInput);

    m_stopBtn = new QPushButton("⏹ Stop");
    m_stopBtn->setFixedHeight(32);
    m_stopBtn->setStyleSheet(
        "QPushButton { background-color: #d20f39; color: white; border: none; "
        "border-radius: 8px; padding: 4px 14px; font-weight: bold; font-size: 11pt; }"
        "QPushButton:hover { background-color: #e64553; }");
    m_stopBtn->hide();
    connect(m_stopBtn, &QPushButton::clicked, this, &MainWindow::stopWorker);
    inputLayout->addWidget(m_stopBtn);

    rightSplitter->addWidget(inputRow);
    rightSplitter->setStretchFactor(0, 1);

    // Main splitter
    auto* mainSplitter = new QSplitter(Qt::Horizontal);
    mainSplitter->addWidget(leftSplitter);
    mainSplitter->addWidget(rightSplitter);
    mainSplitter->setStretchFactor(0, 0);
    mainSplitter->setStretchFactor(1, 1);
    mainSplitter->setSizes({300, 800});
    mainLayout->addWidget(mainSplitter);
}

void MainWindow::updateLlmClient() {
    QString ua = m_config.value("user_agent").toString("PengyAgent/1.0");
    int timeout = m_config.value("tool_timeout").toInt(60);
    pengy_tool_set_user_agent(ua.toUtf8().constData());
    pengy_tool_set_timeout(timeout);
}

void MainWindow::loadChatList() {
    char* json = pengy_chats_load();
    m_chats = QJsonDocument::fromJson(QByteArray(json)).array();
    pengy_free(json);
    m_chatHistory->loadChats(m_chats);
}

void MainWindow::createNewChat() {
    char* json = pengy_chat_create("New Chat");
    if (json) {
        QJsonObject chat = QJsonDocument::fromJson(QByteArray(json)).object();
        pengy_free(json);
        m_currentChat = chat;
        m_currentChatId = chat["id"].toString();
        loadChatList();
        m_chatHistory->selectChatById(m_currentChatId);
        m_chatView->clear();
    }
}

void MainWindow::deleteChat(const QString& chatId) {
    pengy_chat_delete(chatId.toUtf8().constData());
    loadChatList();
    if (m_currentChatId == chatId) {
        if (!m_chats.isEmpty()) {
            loadChat(m_chats[0].toObject()["id"].toString());
        } else {
            createNewChat();
        }
    }
}

void MainWindow::loadChat(const QString& chatId) {
    char* json = pengy_chat_get(chatId.toUtf8().constData());
    if (!json) return;

    QJsonObject chat = QJsonDocument::fromJson(QByteArray(json)).object();
    pengy_free(json);
    m_currentChat = chat;
    m_currentChatId = chatId;

    m_chatHistory->selectChatById(chatId);
    m_chatView->clear();

    QJsonArray messages = chat["messages"].toArray();
    for (const QJsonValue& v : messages) {
        QJsonObject msg = v.toObject();
        QString role = msg["role"].toString();
        if (role == "user") {
            m_chatView->appendMessageText("user", msg["content"].toString());
        } else if (role == "assistant") {
            QJsonArray toolCalls = msg["tool_calls"].toArray();
            if (!toolCalls.isEmpty()) {
                for (const QJsonValue& tc : toolCalls) {
                    QJsonObject tcObj = tc.toObject();
                    QJsonObject fn = tcObj["function"].toObject();
                    QJsonObject args = QJsonDocument::fromJson(
                        fn["arguments"].toString().toUtf8()).object();
                    QJsonObject req;
                    req["tool_call_id"] = tcObj["id"];
                    req["name"] = fn["name"];
                    req["args"] = args;
                    m_chatView->appendMessage("tool_request", req);
                }
                if (!msg["content"].toString().isEmpty()) {
                    m_chatView->appendMessageText("assistant", msg["content"].toString());
                }
            } else if (!msg["content"].toString().isEmpty()) {
                m_chatView->appendMessageText("assistant", msg["content"].toString());
            }
        } else if (role == "tool") {
            QJsonObject result;
            result["tool_call_id"] = msg["tool_call_id"];
            result["content"] = msg["content"];
            result["declined"] = false;
            m_chatView->appendMessage("tool_result", result);
        }
    }
}

void MainWindow::sendMessage(const QString& text, const QStringList& images) {
    if (m_currentChat.isEmpty()) return;

    m_yoloThisTurn = false;
    m_chatHistory->setThinking(true);
    m_chatHistory->updateTokenUsage(0, 0);

    // Build display content with placeholders for images
    QStringList placeholderParts;
    for (const QString& img : images) {
        QString fname = img.section('/', -1);
        placeholderParts.append(QString("[Image: %1]").arg(fname));
    }
    if (!text.isEmpty()) {
        placeholderParts.append(text);
    }
    QString displayContent = placeholderParts.join("\n");

    // Add user message to chat (persistent/display version with placeholders)
    QJsonObject userMsg;
    userMsg["role"] = "user";
    userMsg["content"] = displayContent;
    QJsonArray messages = m_currentChat["messages"].toArray();
    messages.append(userMsg);
    m_currentChat["messages"] = messages;
    m_chatView->appendMessageText("user", displayContent);

    // Update title from first message
    if (m_currentChat["title"].toString() == "New Chat") {
        QString titleSource = text.isEmpty() ? (images.isEmpty() ? "" : images[0].section('/', -1)) : text;
        QString title = titleSource.left(50);
        if (titleSource.length() > 50) title += "...";
        m_currentChat["title"] = title;
        m_chatHistory->updateChatTitle(m_currentChatId, title);
    }

    // Save
    QByteArray chatJson = QJsonDocument(m_currentChat).toJson(QJsonDocument::Compact);
    pengy_chat_save(chatJson.constData());
    loadChatList();

    m_stopBtn->show();

    // Build message history for the API call — images get real base64 data
    QJsonArray apiMessages;
    QString sysMsg = m_config["system_message"].toString();
    if (!sysMsg.isEmpty()) {
        char* rendered = pengy_config_render(sysMsg.toUtf8().constData());
        QJsonObject sysObj;
        sysObj["role"] = "system";
        sysObj["content"] = QString::fromUtf8(rendered);
        pengy_free(rendered);
        apiMessages.append(sysObj);
    }

    // Prior messages use stored (placeholder) content — all except the last
    QJsonArray prior;
    for (int i = 0; i < messages.size() - 1; ++i) {
        prior.append(messages[i]);
    }
    QByteArray priorJson = QJsonDocument(prior).toJson(QJsonDocument::Compact);
    char* cleaned = pengy_clean_messages(priorJson.constData());
    QJsonArray cleanedMsgs = QJsonDocument::fromJson(QByteArray(cleaned)).array();
    pengy_free(cleaned);

    for (const QJsonValue& v : cleanedMsgs) apiMessages.append(v);

    // Build the current user message — with image data if present
    if (!images.isEmpty()) {
        QJsonArray contentParts;
        for (const QString& imgPath : images) {
            QFile imgFile(imgPath);
            if (imgFile.open(QIODevice::ReadOnly)) {
                QByteArray imgData = imgFile.readAll();
                imgFile.close();
                QString b64 = QString::fromUtf8(imgData.toBase64());

                // Detect MIME type
                QMimeDatabase mimeDb;
                QMimeType mime = mimeDb.mimeTypeForFile(imgPath);
                QString mimeStr = mime.name();
                if (mimeStr.isEmpty() || !mimeStr.startsWith("image/")) {
                    // Fall back to extension-based detection
                    QString ext = imgPath.section('.', -1).toLower();
                    if (ext == "jpg" || ext == "jpeg") mimeStr = "image/jpeg";
                    else if (ext == "png") mimeStr = "image/png";
                    else if (ext == "gif") mimeStr = "image/gif";
                    else if (ext == "webp") mimeStr = "image/webp";
                    else mimeStr = "image/jpeg"; // fallback
                }

                QJsonObject imgPart;
                imgPart["type"] = "image_url";
                QJsonObject imgUrlObj;
                imgUrlObj["url"] = QString("data:%1;base64,%2").arg(mimeStr, b64);
                imgPart["image_url"] = imgUrlObj;
                contentParts.append(imgPart);
            }
        }
        if (!text.isEmpty()) {
            QJsonObject textPart;
            textPart["type"] = "text";
            textPart["text"] = text;
            contentParts.append(textPart);
        }
        QJsonObject multimodalMsg;
        multimodalMsg["role"] = "user";
        multimodalMsg["content"] = contentParts;
        apiMessages.append(multimodalMsg);
    } else {
        QJsonObject textMsg;
        textMsg["role"] = "user";
        textMsg["content"] = displayContent;
        apiMessages.append(textMsg);
    }

    processResponse(apiMessages);
}

void MainWindow::processResponse(const QJsonArray& apiMessages) {
    // Cancel any existing worker
    if (m_worker) {
        disconnect(m_worker, nullptr, this, nullptr);
        m_worker->cancel();
        if (m_workerThread) {
            m_workerThread->quit();
            m_workerThread->wait(1000);
            m_workerThread->deleteLater();
            m_workerThread = nullptr;
        }
        m_worker->deleteLater();
        m_worker = nullptr;
    }

    // apiMessages is already pre-built by sendMessage with system prompt,
    // cleaned prior messages, and the current user message (with images if any).

    // Start worker on a new thread
    m_worker = new ChatWorker;
    m_workerThread = new QThread;
    m_worker->moveToThread(m_workerThread);

    connect(m_workerThread, &QThread::started, m_worker, [this, apiMessages]() {
        m_worker->start(
            m_config["base_url"].toString(),
            m_config["api_key"].toString(),
            m_config["model"].toString(),
            apiMessages,
            m_config["tool_confirmation"].toString("none")
        );
    });

    connect(m_worker, &ChatWorker::eventReceived, this, &MainWindow::onWorkerEvent,
            Qt::QueuedConnection);
    connect(m_worker, &ChatWorker::finished, this, &MainWindow::onWorkerFinished,
            Qt::QueuedConnection);
    connect(m_worker, &ChatWorker::errorOccurred, this, &MainWindow::onWorkerError,
            Qt::QueuedConnection);

    m_workerThread->start();
    m_confirmTimer->start();
}

void MainWindow::onWorkerEvent(const QString& eventJson) {
    auto* s = qobject_cast<ChatWorker*>(sender());
    if (s && s != m_worker) return;

    QJsonObject event = QJsonDocument::fromJson(eventJson.toUtf8()).object();
    QString type = event["type"].toString();

    if (type == "final_response") {
        QString content = event["content"].toString();
        QJsonObject usage = event["usage"].toObject();

        QJsonObject asstMsg;
        asstMsg["role"] = "assistant";
        asstMsg["content"] = content;
        QJsonArray messages = m_currentChat["messages"].toArray();
        messages.append(asstMsg);
        m_currentChat["messages"] = messages;

        m_chatView->appendMessageText("assistant", content);
        m_chatHistory->setThinking(false);
        m_chatHistory->updateTokenUsage(
            usage["prompt_tokens"].toInt(),
            usage["completion_tokens"].toInt()
        );

        QByteArray chatJson = QJsonDocument(m_currentChat).toJson(QJsonDocument::Compact);
        pengy_chat_save(chatJson.constData());
        loadChatList();

    } else if (type == "tool_request") {
        m_chatView->appendMessage("tool_request", event);
        QString name = event["name"].toString();
        QString tc = m_config["tool_confirmation"].toString("none");

        bool skipConfirm = (tc == "all") || m_yoloThisTurn ||
            (tc == "safe" && pengy_tool_is_readonly(name.toUtf8().constData()));

        if (skipConfirm) {
            m_worker->sendConfirmation(true, false);
        } else {
            handleToolConfirm(event);
        }

    } else if (type == "assistant_tool_calls") {
        m_yoloThisTurn = false;
        QJsonObject msg = event["message"].toObject();
        QJsonArray messages = m_currentChat["messages"].toArray();
        messages.append(msg);
        m_currentChat["messages"] = messages;

    } else if (type == "tool_result") {
        m_chatView->appendMessage("tool_result", event);
        QJsonObject toolMsg;
        toolMsg["role"] = "tool";
        toolMsg["tool_call_id"] = event["tool_call_id"];
        toolMsg["content"] = event["content"];
        QJsonArray messages = m_currentChat["messages"].toArray();
        messages.append(toolMsg);
        m_currentChat["messages"] = messages;
    }
}

void MainWindow::handleToolConfirm(const QJsonObject& req) {
    // The worker thread will block on the confirm_state.
    // We show a dialog and send the result back.
    QDialog dlg(this);
    dlg.setWindowTitle("Confirm Tool: " + req["name"].toString());
    dlg.setModal(true);
    dlg.resize(480, 300);

    auto* layout = new QVBoxLayout(&dlg);
    QString info = QString("Execute tool: <b>%1</b>\n\nArguments:\n%2")
        .arg(req["name"].toString(),
             QJsonDocument(req["args"].toObject()).toJson(QJsonDocument::Indented));
    auto* label = new QLabel(info);
    label->setWordWrap(true);
    label->setStyleSheet("color: #000; padding: 8px;");
    layout->addWidget(label);

    auto* btnLayout = new QHBoxLayout;
    auto* execBtn = new QPushButton("Execute");
    execBtn->setStyleSheet(
        "QPushButton { background-color: #1e66f5; color: white; border: none; "
        "border-radius: 6px; padding: 8px 18px; font-weight: bold; }"
        "QPushButton:hover { background-color: #4478f7; }");
    auto* yesAllBtn = new QPushButton("Yes to All\nThis Turn");
    yesAllBtn->setStyleSheet(
        "QPushButton { background-color: #df8e1d; color: white; border: none; "
        "border-radius: 6px; padding: 8px 14px; font-weight: bold; }"
        "QPushButton:hover { background-color: #fea82f; }");
    auto* cancelBtn = new QPushButton("Decline");
    cancelBtn->setStyleSheet(
        "QPushButton { background-color: #d20f39; color: white; border: none; "
        "border-radius: 6px; padding: 8px 18px; font-weight: bold; }"
        "QPushButton:hover { background-color: #e64553; }");

    btnLayout->addWidget(execBtn);
    btnLayout->addWidget(yesAllBtn);
    btnLayout->addWidget(cancelBtn);
    layout->addLayout(btnLayout);

    connect(execBtn, &QPushButton::clicked, &dlg, [&]() {
        m_worker->sendConfirmation(true, false);
        dlg.accept();
    });
    connect(yesAllBtn, &QPushButton::clicked, &dlg, [&]() {
        m_yoloThisTurn = true;
        m_worker->sendConfirmation(true, true);
        dlg.accept();
    });
    connect(cancelBtn, &QPushButton::clicked, &dlg, [&]() {
        m_worker->sendConfirmation(false, false);
        dlg.reject();
    });

    dlg.exec();
}

void MainWindow::pollToolConfirmation() {
    // Handled by the QDialog approach above; timer kept for future use
}

void MainWindow::onWorkerFinished() {
    auto* s = qobject_cast<ChatWorker*>(sender());
    if (s && s != m_worker) return;

    m_stopBtn->hide();
    m_chatHistory->setThinking(false);
    m_confirmTimer->stop();

    if (m_workerThread) {
        m_workerThread->quit();
        m_workerThread->wait(1000);
        m_workerThread->deleteLater();
        m_workerThread = nullptr;
    }
    if (m_worker) {
        disconnect(m_worker, nullptr, this, nullptr);
        m_worker->deleteLater();
        m_worker = nullptr;
    }
}

void MainWindow::onWorkerError(const QString& msg) {
    auto* s = qobject_cast<ChatWorker*>(sender());
    if (s && s != m_worker) return;

    m_chatView->appendMessageText("assistant", "Error: " + msg);
    onWorkerFinished();
}

void MainWindow::stopWorker() {
    if (m_worker) m_worker->cancel();
    onWorkerFinished();
    m_chatView->appendMessageText("assistant", "⏹ *Stopped*");
}

void MainWindow::openSettings() {
    SettingsDialog dlg(m_config, this);
    if (dlg.exec() == QDialog::Accepted) {
        m_config = dlg.config();
        QByteArray json = QJsonDocument(m_config).toJson(QJsonDocument::Compact);
        pengy_config_save(json.constData());
        updateLlmClient();
        m_chatHistory->updateQuickSettings(
            m_config["model"].toString(),
            m_config["tool_confirmation"].toString("none"));
    }
}
