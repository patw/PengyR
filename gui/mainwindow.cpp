#include "mainwindow.h"
#include "chathistory.h"
#include "chatview.h"
#include "chatinput.h"
#include "chatworker.h"
#include "settingsdialog.h"
#include "tasksdialog.h"
#include "themehelper.h"
#include "iconhelper.h"
#include <QTabBar>
#include <QToolButton>
#include "pengy_ffi.h"

#include <QSplitter>
#include <QVBoxLayout>
#include <QHBoxLayout>
#include <QPushButton>
#include <QJsonDocument>
#include <QJsonArray>
#include <QJsonObject>
#include <QMessageBox>
#include <QInputDialog>
#include <QLineEdit>
#include <QFile>
#include <QMimeDatabase>
#include <QMimeType>
#include <QCloseEvent>
#include <QScrollArea>
#include <QGroupBox>
#include <QButtonGroup>
#include <QRadioButton>

MainWindow::MainWindow(QWidget* parent) : QMainWindow(parent) {
    // Load config
    char* cfgJson = pengy_config_load();
    QJsonDocument cfgDoc = QJsonDocument::fromJson(QByteArray(cfgJson));
    m_config = cfgDoc.object();
    pengy_free(cfgJson);

    m_runtimeUiScale = m_config["ui_scale"].toInt(100);

    setupUi();
    applyTheme();
    updateLlmClient();
    loadChatList();
    refreshModelCombo();

    // Poll for sudo password requests from any tab's worker
    m_confirmTimer = new QTimer(this);
    m_confirmTimer->setInterval(100);
    connect(m_confirmTimer, &QTimer::timeout, this, &MainWindow::pollToolConfirmation);

    // Restore open tabs from config, or create initial chat
    QJsonArray openArr = m_config["open_tabs"].toArray();
    if (!openArr.isEmpty()) {
        for (const QJsonValue& v : openArr) {
            QString cid = v.toString();
            if (cid.isEmpty()) continue;
            char* json = pengy_chat_get(cid.toUtf8().constData());
            if (json) {
                QJsonObject chat = QJsonDocument::fromJson(QByteArray(json)).object();
                pengy_free(json);
                addTab(chat, (cid == openArr.last().toString()));
            }
        }
    }

    if (m_openTabs.isEmpty()) {
        if (m_chats.isEmpty())
            createNewChat();
        else
            loadIntoNewTab(m_chats[0].toObject()["id"].toString());
    }
}

// ── UI setup ──────────────────────────────────────────────────────

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
    connect(m_chatHistory, &ChatHistoryWidget::tasksRequested, this, &MainWindow::openTasks);
    connect(m_chatHistory, &ChatHistoryWidget::deleteRequested, this, &MainWindow::deleteChat);
    connect(m_chatHistory, &ChatHistoryWidget::modelChanged, this, &MainWindow::onModelChanged);
    leftSplitter->addWidget(m_chatHistory);

    // Right pane: tab widget + input row
    auto* rightSplitter = new QSplitter(Qt::Vertical);

    m_tabWidget = new QTabWidget;
    m_tabWidget->setTabsClosable(false);
    m_tabWidget->tabBar()->setExpanding(false);
    m_tabWidget->setMovable(true);
    m_tabWidget->setUsesScrollButtons(true);
    connect(m_tabWidget, &QTabWidget::tabCloseRequested, this, &MainWindow::closeTab);
    connect(m_tabWidget, &QTabWidget::currentChanged,    this, &MainWindow::onTabChanged);
    rightSplitter->addWidget(m_tabWidget);

    // Input row
    auto* inputRow = new QWidget;
    auto* inputLayout = new QHBoxLayout(inputRow);
    inputLayout->setContentsMargins(8, 4, 8, 4);
    m_chatInput = new ChatInputWidget;
    connect(m_chatInput, &ChatInputWidget::messageSent, this, &MainWindow::sendMessage);
    inputLayout->addWidget(m_chatInput);

    m_stopBtn = new QPushButton("Stop");
    m_stopBtn->setFixedHeight(scaledSize(32, m_runtimeUiScale));
    applyPengyIcon(m_stopBtn, "stop", makeTheme(m_config["theme_mode"].toString("system"), m_config["theme_accent"].toString("default")), 16, "primary_fg", "primary_fg");
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

// ── Theme ─────────────────────────────────────────────────────────

void MainWindow::applyTheme() {
    int themeScale = m_runtimeUiScale;
    QString themeMode = m_config["theme_mode"].toString("system");
    QString themeAccent = m_config["theme_accent"].toString("default");
    Theme theme = makeTheme(themeMode, themeAccent);
    qApp->setFont(scaledSystemFont(m_runtimeUiScale));
    qApp->setStyleSheet(appStyleSheet(theme, themeScale));
    if (m_chatInput) m_chatInput->applyTheme(theme, themeScale);
    if (m_chatHistory) m_chatHistory->applyTheme(theme, themeScale);
    if (m_stopBtn) {
        m_stopBtn->setFixedHeight(scaledSize(32, themeScale));
        m_stopBtn->setStyleSheet(QString(
            "QPushButton { background-color:%1; color:white; border:none; border-radius:8px; padding:4px 14px; font-weight:bold; font-size:11pt; }"
            "QPushButton:hover { background-color:%2; }").arg(theme["danger"], theme["danger_hover"]));
    }
    // Re-theme all open tab chat views
    for (auto& session : m_openTabs) {
        if (session.chatView)
            session.chatView->applyTheme(theme, themeScale);
    }
    for (int i = 0; i < m_tabWidget->count(); ++i) {
        if (auto* button = qobject_cast<QToolButton*>(m_tabWidget->tabBar()->tabButton(i, QTabBar::RightSide))) {
            button->setFixedSize(scaledSize(22, m_runtimeUiScale), scaledSize(22, m_runtimeUiScale));
            applyPengyIcon(button, "close", theme, scaledSize(13, m_runtimeUiScale), "muted", "danger");
        }
    }
}

void MainWindow::updateLlmClient() {
    QString ua = m_config.value("user_agent").toString("PengyAgent/1.0");
    int timeout = m_config.value("tool_timeout").toInt(300);
    int outputMax = m_config.value("tool_output_max_chars").toInt(250000);
    int downloadMax = m_config.value("download_max_mb").toInt(100);
    pengy_tool_set_user_agent(ua.toUtf8().constData());
    pengy_tool_set_timeout(timeout);
    pengy_tool_set_output_max_chars(outputMax);
    pengy_tool_set_download_max_mb(downloadMax);
}

void MainWindow::refreshModelCombo() {
    QByteArray baseUrl = m_config["base_url"].toString().toUtf8();
    char* raw = pengy_models_cached_for(baseUrl.constData());
    QJsonArray arr = QJsonDocument::fromJson(QByteArray(raw)).array();
    pengy_free(raw);

    QStringList models;
    for (const QJsonValue& v : arr) {
        QString id = v.toString();
        if (!id.isEmpty()) models << id;
    }

    TabSession* session = tabForChat(m_activeChatId);
    QString current = session ? modelForSession(session)
                              : m_config["model"].toString("gpt-4o");
    m_chatHistory->setModels(models, current);
}

QString MainWindow::modelForSession(TabSession* session) const {
    if (session) {
        QString overrideModel = session->chat["model"].toString();
        if (!overrideModel.isEmpty())
            return overrideModel;
    }
    return m_config["model"].toString("gpt-4o");
}

void MainWindow::onModelChanged(const QString& model) {
    QString m = model.trimmed();
    TabSession* session = tabForChat(m_activeChatId);
    if (!session || m.isEmpty())
        return;
    if (session->chat["model"].toString() == m)
        return;
    session->chat["model"] = m;
    QByteArray json = QJsonDocument(session->chat).toJson(QJsonDocument::Compact);
    pengy_chat_save(json.constData());
    updateQuickSettingsFor(session);
}

void MainWindow::loadChatList() {
    char* json = pengy_chat_index_load();
    m_chats = QJsonDocument::fromJson(QByteArray(json)).array();
    pengy_free(json);
    m_chatHistory->loadChats(m_chats);
}

// ── Tab management ────────────────────────────────────────────────

TabSession* MainWindow::addTab(const QJsonObject& chat, bool switchTo) {
    auto* chatView = new ChatView;

    TabSession session;
    session.chat     = chat;
    session.chatView = chatView;

    // Apply theme FIRST so renderNow() uses the correct colours
    int themeScale = m_runtimeUiScale;
    Theme theme = makeTheme(m_config["theme_mode"].toString("system"),
                            m_config["theme_accent"].toString("default"));
    chatView->applyTheme(theme, themeScale);

    // Render existing messages
    QJsonArray messages = chat["messages"].toArray();
    for (const QJsonValue& v : messages)
        renderMessage(chatView, v.toObject());
    chatView->renderNow();

    QString chatId = chat["id"].toString();
    m_openTabs[chatId] = session;

    QString title = chat["title"].toString("New Chat").left(30);
    int idx = m_tabWidget->addTab(chatView, title);
    installTabCloseButton(idx);

    if (switchTo)
        m_tabWidget->setCurrentIndex(idx);

    saveOpenTabs();
    return &m_openTabs[chatId];
}

void MainWindow::installTabCloseButton(int index) {
    auto* button = new QToolButton(m_tabWidget->tabBar());
    button->setAutoRaise(true);
    button->setCursor(Qt::ArrowCursor);
    button->setToolTip("Close tab");
    button->setAccessibleName("Close tab");
    button->setFixedSize(scaledSize(22, m_runtimeUiScale), scaledSize(22, m_runtimeUiScale));
    Theme theme = makeTheme(m_config["theme_mode"].toString("system"), m_config["theme_accent"].toString("default"));
    applyPengyIcon(button, "close", theme, scaledSize(13, m_runtimeUiScale), "muted", "danger");
    button->setStyleSheet(QString("QToolButton { background:transparent; border:none; border-radius:5px; padding:3px; } QToolButton:hover { background:%1; }").arg(theme["hover"]));
    connect(button, &QToolButton::clicked, this, [this, button]() {
        QTabBar* bar = m_tabWidget->tabBar();
        for (int i = 0; i < m_tabWidget->count(); ++i)
            if (bar->tabButton(i, QTabBar::RightSide) == button) { closeTab(i); return; }
    });
    m_tabWidget->tabBar()->setTabButton(index, QTabBar::RightSide, button);
}

void MainWindow::closeTab(int index) {
    if (index < 0) return;
    QWidget* w = m_tabWidget->widget(index);
    QString chatId;
    for (auto it = m_openTabs.begin(); it != m_openTabs.end(); ++it) {
        if (it->chatView == w) {
            chatId = it.key();
            break;
        }
    }

    if (chatId.isEmpty()) {
        m_tabWidget->removeTab(index);
        return;
    }

    TabSession& session = m_openTabs[chatId];
    abandonWorkerFor(&session);

    // Save or delete
    bool isEmptyNew = (session.chat["title"].toString() == "New Chat"
                       && session.chat["messages"].toArray().isEmpty());
    if (isEmptyNew)
        pengy_chat_delete(chatId.toUtf8().constData());
    else {
        QByteArray json = QJsonDocument(session.chat).toJson(QJsonDocument::Compact);
        pengy_chat_save(json.constData());
    }

    m_tabWidget->removeTab(index);
    m_openTabs.remove(chatId);
    saveOpenTabs();

    if (m_tabWidget->count() == 0)
        createNewChat();
}

void MainWindow::onTabChanged(int index) {
    if (index < 0) return;
    QWidget* w = m_tabWidget->widget(index);
    for (auto it = m_openTabs.begin(); it != m_openTabs.end(); ++it) {
        if (it->chatView == w) {
            m_activeChatId = it.key();
            m_chatHistory->selectChatById(it.key());
            updateQuickSettingsFor(&it.value());
            m_stopBtn->setVisible(it->thinking);
            return;
        }
    }
}

TabSession* MainWindow::tabForChat(const QString& chatId) {
    auto it = m_openTabs.find(chatId);
    return (it != m_openTabs.end()) ? &it.value() : nullptr;
}

void MainWindow::saveOpenTabs() {
    QJsonArray arr;
    for (const auto& key : m_openTabs.keys())
        arr.append(key);
    m_config["open_tabs"] = arr;
    QByteArray json = QJsonDocument(m_config).toJson(QJsonDocument::Compact);
    pengy_config_save(json.constData());
}

void MainWindow::updateTabTitle(TabSession* session) {
    QString base = session->chat["title"].toString("New Chat").left(30);
    QString prefix = session->thinking ? "● " : "";

    for (int i = 0; i < m_tabWidget->count(); ++i) {
        if (m_tabWidget->widget(i) == session->chatView) {
            m_tabWidget->setTabText(i, prefix + base);
            return;
        }
    }
}

void MainWindow::loadIntoNewTab(const QString& chatId) {
    // If there's a single empty "New Chat" tab, replace it
    if (m_openTabs.size() == 1) {
        QString onlyId = m_openTabs.firstKey();
        TabSession& onlySession = m_openTabs.first();
        if (onlySession.chat["title"].toString() == "New Chat"
            && onlySession.chat["messages"].toArray().isEmpty()) {
            pengy_chat_delete(onlySession.chat["id"].toString().toUtf8().constData());
            for (int i = 0; i < m_tabWidget->count(); ++i) {
                if (m_tabWidget->widget(i) == onlySession.chatView) {
                    m_tabWidget->removeTab(i);
                    break;
                }
            }
            m_openTabs.remove(onlyId);
        }
    }

    char* json = pengy_chat_get(chatId.toUtf8().constData());
    if (!json) return;

    QJsonObject chat = QJsonDocument::fromJson(QByteArray(json)).object();
    pengy_free(json);

    addTab(chat, true);
    m_activeChatId = chatId;
    m_chatHistory->selectChatById(chatId);

    TabSession* session = tabForChat(chatId);
    if (session) updateQuickSettingsFor(session);
}

// ── Message rendering ─────────────────────────────────────────────

// The ChatView display object for an assistant message, carrying whichever
// reasoning field the provider used.
QJsonObject MainWindow::assistantDisplayMessage(const QJsonObject& msg) {
    QJsonObject display;
    display["role"] = "assistant";
    display["content"] = msg["content"].toString();
    if (msg.contains("reasoning_content"))
        display["reasoning_content"] = msg["reasoning_content"];
    else if (msg.contains("reasoning"))
        display["reasoning_content"] = msg["reasoning"];
    return display;
}

void MainWindow::renderMessage(ChatView* view, const QJsonObject& msg) {
    QString role = msg["role"].toString();

    if (role == "user") {
        view->appendMessageText("user", msg["content"].toString(), false);

    } else if (role == "assistant") {
        // Text first, tool cards after: the model wrote its narration *before*
        // deciding on the tool calls in the same message, and that is the order
        // the live run renders it in.
        if (!msg["content"].toString().isEmpty())
            view->appendMessage("assistant", assistantDisplayMessage(msg), false);

        for (const QJsonValue& tc : msg["tool_calls"].toArray()) {
            QJsonObject tcObj = tc.toObject();
            QJsonObject fn = tcObj["function"].toObject();
            QJsonObject args = QJsonDocument::fromJson(
                fn["arguments"].toString().toUtf8()).object();
            QJsonObject req;
            req["tool_call_id"] = tcObj["id"];
            req["name"] = fn["name"];
            req["args"] = args;
            view->appendMessage("tool_request", req, false);
        }
    } else if (role == "tool") {
        QJsonObject result;
        result["tool_call_id"] = msg["tool_call_id"];
        result["content"] = msg["content"];
        result["declined"] = false;
        view->appendMessage("tool_result", result, false);
    }
}

// ── Chat lifecycle ────────────────────────────────────────────────

void MainWindow::createNewChat() {
    // If any open tab is an empty "New Chat", just switch to it
    for (auto it = m_openTabs.begin(); it != m_openTabs.end(); ++it) {
        if (it->chat["title"].toString() == "New Chat"
            && it->chat["messages"].toArray().isEmpty()) {
            for (int i = 0; i < m_tabWidget->count(); ++i) {
                if (m_tabWidget->widget(i) == it->chatView) {
                    m_tabWidget->setCurrentIndex(i);
                    return;
                }
            }
        }
    }

    char* json = pengy_chat_create("New Chat");
    if (!json) return;

    QJsonObject chat = QJsonDocument::fromJson(QByteArray(json)).object();
    pengy_free(json);

    loadChatList();
    addTab(chat, true);
    m_activeChatId = chat["id"].toString();
    m_chatHistory->selectChatById(m_activeChatId);

    TabSession* session = tabForChat(m_activeChatId);
    if (session) updateQuickSettingsFor(session);
}

void MainWindow::deleteChat(const QString& chatId) {
    char* json = pengy_chat_get(chatId.toUtf8().constData());
    QString title = "this chat";
    if (json) {
        QJsonObject chat = QJsonDocument::fromJson(QByteArray(json)).object();
        title = chat["title"].toString("this chat");
        pengy_free(json);
    }
    QMessageBox::StandardButton reply = QMessageBox::question(
        this, "Delete Chat",
        QString("Delete \"%1\"?\n\nThis cannot be undone.").arg(title),
        QMessageBox::Yes | QMessageBox::Cancel,
        QMessageBox::Cancel);
    if (reply != QMessageBox::Yes) return;

    // Close tab if open
    TabSession* session = tabForChat(chatId);
    if (session) {
        abandonWorkerFor(session);
        for (int i = 0; i < m_tabWidget->count(); ++i) {
            if (m_tabWidget->widget(i) == session->chatView) {
                m_tabWidget->removeTab(i);
                break;
            }
        }
        m_openTabs.remove(chatId);
    }

    pengy_chat_delete(chatId.toUtf8().constData());
    loadChatList();

    if (m_tabWidget->count() == 0)
        createNewChat();

    saveOpenTabs();
}

void MainWindow::loadChat(const QString& chatId) {
    TabSession* existing = tabForChat(chatId);
    if (existing) {
        for (int i = 0; i < m_tabWidget->count(); ++i) {
            if (m_tabWidget->widget(i) == existing->chatView) {
                m_tabWidget->setCurrentIndex(i);
                return;
            }
        }
    } else {
        loadIntoNewTab(chatId);
    }
}

void MainWindow::openSettings() {
    SettingsDialog dlg(m_config, this);
    if (dlg.exec() == QDialog::Accepted) {
        m_config = dlg.config();
        QByteArray json = QJsonDocument(m_config).toJson(QJsonDocument::Compact);
        pengy_config_save(json.constData());
        applyTheme();
        updateLlmClient();
        loadChatList();
        refreshModelCombo();
        if (!m_activeChatId.isEmpty())
            m_chatHistory->selectChatById(m_activeChatId);
        TabSession* session = tabForChat(m_activeChatId);
        if (session)
            updateQuickSettingsFor(session);
    }
}

void MainWindow::openTasks() {
    Theme theme = makeTheme(m_config["theme_mode"].toString("system"),
                            m_config["theme_accent"].toString("default"));
    TasksDialog dlg(theme, this);
    connect(&dlg, &TasksDialog::taskPlayed, this, [this](const QString& prompt) {
        sendMessage(prompt, QStringList());
    });
    dlg.exec();
}

// ── Sending messages ──────────────────────────────────────────────

void MainWindow::sendMessage(const QString& text, const QStringList& images) {
    TabSession* session = tabForChat(m_activeChatId);
    if (!session || session->chat.isEmpty()) return;

    session->yoloThisTurn = false;

    // Build display content with placeholders for images
    QStringList placeholderParts;
    for (const QString& img : images) {
        QString fname = img.section('/', -1);
        placeholderParts.append(QString("[Image: %1]").arg(fname));
    }
    if (!text.isEmpty())
        placeholderParts.append(text);
    QString displayContent = placeholderParts.join("\n");

    // Add user message to chat
    QJsonObject userMsg;
    userMsg["role"] = "user";
    userMsg["content"] = displayContent;
    QJsonArray messages = session->chat["messages"].toArray();
    messages.append(userMsg);
    session->chat["messages"] = messages;
    session->chatView->appendMessageText("user", displayContent);

    // Update title from first message
    if (session->chat["title"].toString() == "New Chat") {
        QString titleSource = text.isEmpty()
            ? (images.isEmpty() ? "" : images[0].section('/', -1))
            : text;
        QString title = titleSource.left(50);
        if (titleSource.length() > 50) title += "...";
        session->chat["title"] = title;
        m_chatHistory->updateChatTitle(session->chat["id"].toString(), title);
        updateTabTitle(session);
    }

    // Save
    QByteArray chatJson = QJsonDocument(session->chat).toJson(QJsonDocument::Compact);
    pengy_chat_save(chatJson.constData());

    session->thinking = true;
    updateTabTitle(session);
    updateQuickSettingsFor(session);
    m_stopBtn->show();

    // Build API message list
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

    // Prior messages (all but last), cleaned + elided
    QJsonArray prior;
    for (int i = 0; i < messages.size() - 1; ++i)
        prior.append(messages[i]);

    QByteArray priorJson = QJsonDocument(prior).toJson(QJsonDocument::Compact);
    char* cleaned = pengy_clean_messages(priorJson.constData());
    QJsonArray cleanedMsgs = QJsonDocument::fromJson(QByteArray(cleaned)).array();
    pengy_free(cleaned);
    for (const QJsonValue& v : cleanedMsgs) apiMessages.append(v);

    // Current user message (with real image data if any)
    if (!images.isEmpty()) {
        int maxDim = m_config.value("image_max_dimension").toInt(4096);
        double maxMb = m_config.value("image_max_mb").toDouble(4.5);
        int quality = m_config.value("image_quality").toInt(85);

        QJsonArray contentParts;
        for (const QString& imgPath : images) {
            char* result = pengy_image_preprocess(
                imgPath.toUtf8().constData(),
                static_cast<unsigned int>(maxDim),
                maxMb,
                static_cast<unsigned char>(quality));
            if (result) {
                QJsonObject preprocessed = QJsonDocument::fromJson(QByteArray(result)).object();
                pengy_free(result);
                QString b64 = preprocessed["bytes_base64"].toString();
                QString mime = preprocessed["mime"].toString("image/jpeg");

                QJsonObject imgPart;
                imgPart["type"] = "image_url";
                QJsonObject imgUrlObj;
                imgUrlObj["url"] = QString("data:%1;base64,%2").arg(mime, b64);
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

    processResponse(session, apiMessages);
}

void MainWindow::processResponse(TabSession* session, const QJsonArray& apiMessages) {
    abandonWorkerFor(session);

    auto* worker = new ChatWorker;
    auto* thread = new QThread;
    worker->moveToThread(thread);

    QString chatId = session->chat["id"].toString();
    m_workerToChat[worker] = chatId;

    QString baseUrl = m_config["base_url"].toString();
    QString apiKey = m_config["api_key"].toString();
    QString model = modelForSession(session);
    QString tc = m_config["tool_confirmation"].toString("none");
    QString re = m_config["reasoning_effort"].toString("");
    bool preserveReasoning = m_config["preserve_reasoning"].toBool(false);

    connect(thread, &QThread::started, worker, [worker, baseUrl, apiKey, model,
            apiMessages, tc, re, preserveReasoning]() {
        worker->start(baseUrl, apiKey, model, apiMessages, tc, re, preserveReasoning);
    });

    connect(worker, &ChatWorker::eventReceived, this, &MainWindow::onWorkerEvent,
            Qt::QueuedConnection);
    connect(worker, &ChatWorker::finished, this, &MainWindow::onWorkerFinished,
            Qt::QueuedConnection);
    connect(worker, &ChatWorker::errorOccurred, this, &MainWindow::onWorkerError,
            Qt::QueuedConnection);

    thread->start();

    session->worker = worker;
    session->workerThread = thread;
    m_confirmTimer->start();
}

// ── Worker signal handlers ────────────────────────────────────────

void MainWindow::onWorkerEvent(const QString& eventJson) {
    auto* worker = qobject_cast<ChatWorker*>(sender());
    if (!worker) return;

    QString chatId = m_workerToChat.value(worker);
    if (chatId.isEmpty()) return;

    TabSession* session = tabForChat(chatId);
    if (!session) return;

    QJsonObject event = QJsonDocument::fromJson(eventJson.toUtf8()).object();
    QString type = event["type"].toString();

    if (type == "final_response") {
        handleFinalResponse(session, event);

    } else if (type == "tool_request") {
        session->thinking = true;
        session->toolRunning = true;
        updateTabTitle(session);
        // Refresh the status dot *before* unblocking the worker below —
        // setToolRunning() forces an immediate repaint so the orange state
        // is actually visible for auto-approved tools.
        if (session == tabForChat(m_activeChatId))
            updateQuickSettingsFor(session);
        session->chatView->appendMessage("tool_request", event);

        QString name = event["name"].toString();
        QString tc = m_config["tool_confirmation"].toString("none");
        bool skipConfirm = (tc == "all") || session->yoloThisTurn ||
            (tc == "safe" && pengy_tool_is_readonly(name.toUtf8().constData()));

        if (skipConfirm) {
            worker->sendConfirmation(true, false);
        } else {
            handleToolConfirm(session, event);
        }

    } else if (type == "assistant_tool_calls") {
        session->yoloThisTurn = false;
        QJsonObject msg = event["message"].toObject();
        // The narration the model wrote alongside its tool calls is persisted
        // and shows on reload, so it has to render live too -- before the tool
        // cards, which is the order it was written in.
        if (!msg["content"].toString().trimmed().isEmpty())
            session->chatView->appendMessage("assistant", assistantDisplayMessage(msg));
        QJsonArray messages = session->chat["messages"].toArray();
        messages.append(msg);
        session->chat["messages"] = messages;

    } else if (type == "tool_result") {
        session->toolRunning = false;
        session->thinking = true;
        updateTabTitle(session);
        if (session == tabForChat(m_activeChatId))
            updateQuickSettingsFor(session);
        session->chatView->appendMessage("tool_result", event);
        QJsonObject toolMsg;
        toolMsg["role"] = "tool";
        toolMsg["tool_call_id"] = event["tool_call_id"];
        toolMsg["content"] = event["content"];
        QJsonArray messages = session->chat["messages"].toArray();
        messages.append(toolMsg);
        session->chat["messages"] = messages;
    }
}

void MainWindow::handleFinalResponse(TabSession* session, const QJsonObject& response) {
    QString content = response["content"].toString();

    if (!content.isEmpty()) {
        QJsonObject asstMsg = response["message"].toObject();
        if (asstMsg.isEmpty()) {
            asstMsg["role"] = "assistant";
            asstMsg["content"] = content;
        }
        QJsonArray messages = session->chat["messages"].toArray();
        messages.append(asstMsg);
        session->chat["messages"] = messages;

        QJsonObject display;
        display["role"] = "assistant";
        display["content"] = content;
        if (asstMsg.contains("reasoning_content"))
            display["reasoning_content"] = asstMsg["reasoning_content"];
        else if (asstMsg.contains("reasoning"))
            display["reasoning_content"] = asstMsg["reasoning"];
        session->chatView->appendMessage("assistant", display);

        QByteArray chatJson = QJsonDocument(session->chat).toJson(QJsonDocument::Compact);
        pengy_chat_save(chatJson.constData());
    }

    QJsonObject usage = response["usage"].toObject();
    session->promptTokens = usage["prompt_tokens"].toInt();
    session->completionTokens = usage["completion_tokens"].toInt();

    if (session == tabForChat(m_activeChatId))
        updateQuickSettingsFor(session);
}

void MainWindow::handleToolConfirm(TabSession* session, const QJsonObject& req) {
    int themeScale = m_runtimeUiScale;
    Theme theme = makeTheme(m_config["theme_mode"].toString("system"),
                            m_config["theme_accent"].toString("default"));
    QDialog dlg(this);
    dlg.setWindowTitle("Confirm Tool: " + req["name"].toString());
    dlg.setModal(true);
    dlg.resize(480, 320);
    dlg.setMaximumWidth(600);
    dlg.setStyleSheet(appStyleSheet(theme, themeScale));

    auto* layout = new QVBoxLayout(&dlg);
    auto* header = new QLabel(QString("Execute tool: <b>%1</b>").arg(req["name"].toString()));
    header->setStyleSheet(QString("color:%1; padding:8px;").arg(theme["fg"]));
    layout->addWidget(header);

    auto* argsLabel = new QLabel("Arguments:");
    argsLabel->setStyleSheet(QString("color:%1; padding:0 8px;").arg(theme["fg"]));
    layout->addWidget(argsLabel);

    QString argsText = QJsonDocument(req["args"].toObject()).toJson(QJsonDocument::Indented);
    static const int kMaxArgsLen = 4000;
    if (argsText.length() > kMaxArgsLen) {
        argsText = argsText.left(kMaxArgsLen) + QString("\n... [truncated, %1 chars total]").arg(argsText.length());
    }
    auto* argsEdit = new QPlainTextEdit(argsText);
    argsEdit->setReadOnly(true);
    argsEdit->setLineWrapMode(QPlainTextEdit::WidgetWidth);
    argsEdit->setWordWrapMode(QTextOption::WrapAnywhere);
    argsEdit->setStyleSheet(QString("color:%1; padding:4px;").arg(theme["fg"]));
    layout->addWidget(argsEdit, 1);

    auto* btnLayout = new QHBoxLayout;
    auto* execBtn = new QPushButton("Execute");
    execBtn->setStyleSheet(QString(
        "QPushButton { background-color:%1; color:%2; border:none; border-radius:6px; padding:8px 18px; font-weight:bold; }"
        "QPushButton:hover { background-color:%3; }").arg(theme["primary"], theme["primary_fg"], theme["primary_hover"]));
    auto* yesAllBtn = new QPushButton("Yes to All\nThis Turn");
    yesAllBtn->setStyleSheet(QString(
        "QPushButton { background-color:%1; color:white; border:none; border-radius:6px; padding:8px 14px; font-weight:bold; }"
        "QPushButton:hover { background-color:%2; }").arg(theme["warning"], theme["warning_hover"]));
    auto* cancelBtn = new QPushButton("Decline");
    cancelBtn->setStyleSheet(QString(
        "QPushButton { background-color:%1; color:white; border:none; border-radius:6px; padding:8px 18px; font-weight:bold; }"
        "QPushButton:hover { background-color:%2; }").arg(theme["danger"], theme["danger_hover"]));

    btnLayout->addWidget(execBtn);
    btnLayout->addWidget(yesAllBtn);
    btnLayout->addWidget(cancelBtn);
    layout->addLayout(btnLayout);

    ChatWorker* worker = session->worker;
    bool responded = false;
    connect(execBtn, &QPushButton::clicked, &dlg, [&]() {
        responded = true;
        if (worker) worker->sendConfirmation(true, false);
        dlg.accept();
    });
    connect(yesAllBtn, &QPushButton::clicked, &dlg, [&]() {
        responded = true;
        session->yoloThisTurn = true;
        if (worker) worker->sendConfirmation(true, true);
        dlg.accept();
    });
    connect(cancelBtn, &QPushButton::clicked, &dlg, [&]() {
        responded = true;
        if (worker) worker->sendConfirmation(false, false);
        dlg.reject();
    });

    dlg.exec();

    if (!responded && worker)
        worker->sendConfirmation(false, false);
}

void MainWindow::onWorkerError(const QString& msg) {
    auto* worker = qobject_cast<ChatWorker*>(sender());
    if (!worker) return;

    QString chatId = m_workerToChat.value(worker);
    TabSession* session = tabForChat(chatId);
    if (!session) return;

    session->chatView->appendMessageText("assistant", "Error: " + msg);
    session->thinking = false;
    session->toolRunning = false;
    updateTabTitle(session);

    if (session == tabForChat(m_activeChatId)) {
        m_stopBtn->hide();
        updateQuickSettingsFor(session);
    }
}

void MainWindow::onWorkerFinished() {
    auto* worker = qobject_cast<ChatWorker*>(sender());
    if (!worker) return;

    QString chatId = m_workerToChat.take(worker);
    TabSession* session = tabForChat(chatId);
    if (!session) {
        // Stale worker — clean up
        worker->deleteLater();
        return;
    }

    if (session->workerThread && session->workerThread->isRunning()) {
        session->workerThread->quit();
        session->workerThread->wait(5000);
    }
    session->workerThread = nullptr;
    session->worker = nullptr;
    session->thinking = false;
    session->toolRunning = false;
    updateTabTitle(session);

    if (session == tabForChat(m_activeChatId)) {
        m_stopBtn->hide();
        m_confirmTimer->stop();
        updateQuickSettingsFor(session);
    }

    disconnect(worker, nullptr, this, nullptr);
    worker->deleteLater();
}

// ── Worker lifecycle ──────────────────────────────────────────────

void MainWindow::abandonWorkerFor(TabSession* session) {
    if (!session->worker) return;

    m_workerToChat.remove(session->worker);
    session->worker->cancel();

    try {
        disconnect(session->worker, nullptr, this, nullptr);
    } catch (...) {}

    if (session->workerThread) {
        session->workerThread->quit();
        m_abandonedWorkers.append({session->workerThread, session->worker});
        connect(session->workerThread, &QThread::finished,
                this, &MainWindow::reapAbandonedWorkers);
    }

    session->worker = nullptr;
    session->workerThread = nullptr;
}

void MainWindow::reapAbandonedWorkers() {
    m_abandonedWorkers.erase(
        std::remove_if(m_abandonedWorkers.begin(), m_abandonedWorkers.end(),
                       [](const AbandonedWorker& aw) {
                           if (!aw.thread->isRunning()) {
                               aw.thread->deleteLater();
                               aw.worker->deleteLater();
                               return true;
                           }
                           return false;
                       }),
        m_abandonedWorkers.end());
}

void MainWindow::stopWorker() {
    TabSession* session = tabForChat(m_activeChatId);
    if (!session) return;

    abandonWorkerFor(session);

    m_stopBtn->hide();
    m_confirmTimer->stop();
    session->thinking = false;
    session->toolRunning = false;
    updateTabTitle(session);
    updateQuickSettingsFor(session);

    if (!session->chat.isEmpty()) {
        QByteArray priorJson = QJsonDocument(session->chat["messages"].toArray())
                                   .toJson(QJsonDocument::Compact);
        char* cleaned = pengy_clean_messages(priorJson.constData());
        session->chat["messages"] = QJsonDocument::fromJson(QByteArray(cleaned)).array();
        pengy_free(cleaned);
        session->chatView->appendMessageText("assistant", "⏹ *Stopped*");
        QByteArray json = QJsonDocument(session->chat).toJson(QJsonDocument::Compact);
        pengy_chat_save(json.constData());
    }
}

void MainWindow::handleQuestionRequest(TabSession* session) {
    ChatWorker* worker = session->worker;
    if (!worker) return;

    // Parse questions from the shared buffer
    QByteArray qJson = worker->questionJson();
    QJsonDocument qDoc = QJsonDocument::fromJson(qJson);
    QJsonArray questions = qDoc.array();
    if (questions.isEmpty()) {
        worker->sendQuestionAnswers(QStringList());
        return;
    }

    int themeScale = m_runtimeUiScale;
    Theme theme = makeTheme(m_config["theme_mode"].toString("system"),
                            m_config["theme_accent"].toString("default"));

    QDialog dlg(this);
    dlg.setWindowTitle("Pengy — Questions");
    dlg.setModal(true);
    dlg.setMinimumWidth(450);
    dlg.setMaximumWidth(750);
    dlg.setStyleSheet(appStyleSheet(theme, themeScale));

    QVBoxLayout* layout = new QVBoxLayout(&dlg);
    QLabel* header = new QLabel("The assistant needs your input:");
    header->setStyleSheet(QString("color:%1; font-weight:bold; padding:4px;").arg(theme["fg"]));
    layout->addWidget(header);

    QVector<QButtonGroup*> groups;
    QScrollArea* scrollArea = new QScrollArea;
    scrollArea->setWidgetResizable(true);
    QWidget* scrollW = new QWidget;
    QVBoxLayout* scrollL = new QVBoxLayout(scrollW);
    scrollL->setSizeConstraint(QLayout::SetMinAndMaxSize);

    for (int qi = 0; qi < questions.size(); ++qi) {
        QJsonObject q = questions[qi].toObject();
        QString headerText = q["header"].toString();
        QString questionText = q["question"].toString();
        QJsonArray opts = q["options"].toArray();

        QGroupBox* gb = new QGroupBox(headerText);
        gb->setStyleSheet(QString(
            "QGroupBox { color:%1; font-weight:bold; border:1px solid %2; "
            "border-radius:6px; margin-top:8px; padding:12px 8px 8px 8px; } "
            "QGroupBox::title { subcontrol-origin:margin; left:10px; padding:0 4px; }")
            .arg(theme["primary"], theme["border_soft"]));
        QVBoxLayout* gl = new QVBoxLayout(gb);
        QLabel* qLabel = new QLabel(questionText);
        qLabel->setWordWrap(true);
        gl->addWidget(qLabel);
        QButtonGroup* bg = new QButtonGroup(&dlg);
        groups.append(bg);

        for (int oi = 0; oi < opts.size(); ++oi) {
            QJsonObject opt = opts[oi].toObject();
            QRadioButton* rb = new QRadioButton(
                QString("%1  —  %2")
                    .arg(opt["label"].toString(), opt["description"].toString()));
            bg->addButton(rb, oi);
            gl->addWidget(rb);
            if (oi == 0) rb->setChecked(true);
        }
        scrollL->addWidget(gb);
    }
    scrollL->addStretch();
    scrollArea->setWidget(scrollW);
    layout->addWidget(scrollArea, 1);

    QHBoxLayout* btnL = new QHBoxLayout;
    QPushButton* submit = new QPushButton("Submit Answers");
    submit->setStyleSheet(QString(
        "QPushButton{background-color:%1;color:%2;border:none;border-radius:6px;"
        "padding:8px 24px;font-weight:bold;}")
        .arg(theme["primary"], theme["primary_fg"]));
    QPushButton* cancel = new QPushButton("Cancel");
    cancel->setStyleSheet(QString(
        "QPushButton{background-color:%1;color:white;border:none;border-radius:6px;"
        "padding:8px 24px;font-weight:bold;}")
        .arg(theme["danger"]));
    btnL->addWidget(submit);
    btnL->addWidget(cancel);
    layout->addLayout(btnL);

    // Size dialog to fit content, with reasonable limits
    scrollW->adjustSize();
    int idealW = qMin(scrollW->sizeHint().width() + 40, 750);
    int idealH = qMin(scrollW->sizeHint().height() + 140, 650);
    dlg.resize(qMax(idealW, 480), qMax(idealH, 280));

    connect(submit, &QPushButton::clicked, &dlg, &QDialog::accept);
    connect(cancel, &QPushButton::clicked, &dlg, &QDialog::reject);

    if (dlg.exec() == QDialog::Accepted) {
        QStringList answers;
        for (auto* bg : groups) {
            QAbstractButton* checked = bg->checkedButton();
            if (checked) {
                QString text = checked->text();
                int dash = text.indexOf("  —  ");
                answers.append(dash > 0 ? text.left(dash) : text);
            } else {
                answers.append("");
            }
        }
        worker->sendQuestionAnswers(answers);
    } else {
        worker->cancelQuestion();
    }
}

void MainWindow::pollToolConfirmation() {
    TabSession* session = tabForChat(m_activeChatId);
    if (!session || !session->worker || m_sudoDialogOpen) return;

    // Check for question request first
    if (session->worker->isQuestionPending()) {
        m_sudoDialogOpen = true;
        handleQuestionRequest(session);
        m_sudoDialogOpen = false;
        return;
    }

    if (!session->worker->isSudoPending()) return;

    m_sudoDialogOpen = true;

    QString password;
    bool ok = false;
    password = QInputDialog::getText(
        this, "sudo Password", "Enter sudo password:",
        QLineEdit::Password, QString(), &ok);

    m_sudoDialogOpen = false;

    if (ok && !password.isEmpty())
        session->worker->sendSudoPassword(password);
    else
        session->worker->cancelSudo();
}

// ── Quick settings panel ──────────────────────────────────────────

void MainWindow::updateQuickSettingsFor(TabSession* session) {
    m_chatHistory->updateQuickSettings(
        modelForSession(session),
        m_config["tool_confirmation"].toString("none"));

    if (session->promptTokens || session->completionTokens)
        m_chatHistory->updateTokenUsage(session->promptTokens, session->completionTokens);

    if (session->toolRunning)
        m_chatHistory->setToolRunning(true);
    else if (session->thinking)
        m_chatHistory->setThinking(true);
    else
        m_chatHistory->setThinking(false);
}

// ── Clean shutdown ────────────────────────────────────────────────

void MainWindow::closeEvent(QCloseEvent* event) {
    saveOpenTabs();
    for (auto& session : m_openTabs) {
        if (!session.chat.isEmpty()) {
            QByteArray json = QJsonDocument(session.chat).toJson(QJsonDocument::Compact);
            pengy_chat_save(json.constData());
        }
    }

    // Cancel every live worker (open tabs + already-abandoned ones) and wait
    // for its thread to stop, so no QThread is destroyed while still running.
    QList<QThread*> threads;
    for (auto& session : m_openTabs) {
        if (session.worker) session.worker->cancel();
        if (session.workerThread) threads.append(session.workerThread);
    }
    for (const auto& aw : m_abandonedWorkers) {
        if (aw.worker) aw.worker->cancel();
        if (aw.thread) threads.append(aw.thread);
    }
    for (QThread* t : threads) {
        if (t && t->isRunning()) t->quit();
    }
    for (QThread* t : threads) {
        if (t && t->isRunning()) t->wait(3000);
    }

    QMainWindow::closeEvent(event);
}
