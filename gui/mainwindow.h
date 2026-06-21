#pragma once
#include <QMainWindow>
#include <QJsonObject>
#include <QJsonArray>
#include <QTimer>
#include <QPushButton>
#include <QDialog>
#include <QLabel>
#include <QVBoxLayout>
#include <QHBoxLayout>
#include <QThread>

class ChatHistoryWidget;
class ChatView;
class ChatInputWidget;
class ChatWorker;
class SettingsDialog;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    explicit MainWindow(QWidget* parent = nullptr);

private slots:
    void createNewChat();
    void loadChat(const QString& chatId);
    void deleteChat(const QString& chatId);
    void sendMessage(const QString& text, const QStringList& images);
    void openSettings();
    void onWorkerEvent(const QString& eventJson);
    void onWorkerFinished();
    void onWorkerError(const QString& msg);
    void stopWorker();
    void pollToolConfirmation();

private:
    void setupUi();
    void updateLlmClient();
    void loadChatList();
    void processResponse(const QJsonArray& messages);
    void handleToolConfirm(const QJsonObject& toolRequest);

    QJsonObject m_config;
    QJsonArray m_chats;
    QString m_currentChatId;
    QJsonObject m_currentChat;

    ChatHistoryWidget* m_chatHistory;
    ChatView* m_chatView;
    ChatInputWidget* m_chatInput;
    QPushButton* m_stopBtn;

    ChatWorker* m_worker = nullptr;
    QThread* m_workerThread = nullptr;
    QTimer* m_confirmTimer = nullptr;
    bool m_yoloThisTurn = false;
};
