#pragma once
#include <QObject>
#include <QThread>
#include <QMutex>
#include <QWaitCondition>
#include <QJsonObject>
#include <QJsonArray>
#include "pengy_ffi.h"

/// Mirrors Python's ChatWorker — runs the LLM chat in a background QThread.
/// Uses the Rust library's pengy_llm_chat_run() with a spin-wait for
/// tool confirmations (same pattern as Python's threading.Event).
class ChatWorker : public QObject {
    Q_OBJECT
public:
    explicit ChatWorker(QObject* parent = nullptr);
    ~ChatWorker();

    void start(const QString& baseUrl, const QString& apiKey,
               const QString& model, const QJsonArray& messages,
               const QString& toolConfirmation);

    void cancel();
    void sendConfirmation(bool confirmed, bool yoloTurn);

signals:
    void eventReceived(const QString& eventJson);
    void finished();
    void errorOccurred(const QString& message);

private:
    void run();

    // Shared state with Qt main thread for tool confirmation
    ConfirmState m_confirmState;
    QMutex m_mutex;
    QWaitCondition m_cond;
    bool m_cancelled = false;

    // Parameters
    QString m_baseUrl, m_apiKey, m_model, m_messagesJson, m_toolConfirmation;
};
