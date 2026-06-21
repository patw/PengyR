#include "chatworker.h"
#include <QDebug>

ChatWorker::ChatWorker(QObject* parent) : QObject(parent) {
    m_confirmState.status = 0;
    m_confirmState.yolo_turn = false;
}

ChatWorker::~ChatWorker() { cancel(); }

void ChatWorker::start(const QString& baseUrl, const QString& apiKey,
                       const QString& model, const QJsonArray& messages,
                       const QString& toolConfirmation) {
    m_baseUrl = baseUrl;
    m_apiKey = apiKey;
    m_model = model;
    m_messagesJson = QJsonDocument(messages).toJson(QJsonDocument::Compact);
    m_toolConfirmation = toolConfirmation;
    m_cancelled = false;
    m_confirmState.status = 0;

    // Run on a QThread (auto-deleted on finish)
    auto* thread = QThread::create([this] { run(); });
    connect(thread, &QThread::finished, thread, &QObject::deleteLater);
    thread->start();
}

void ChatWorker::cancel() {
    m_cancelled = true;
    pengy_llm_cancel(nullptr);

    // Wake up any waiting confirmation
    QMutexLocker lock(&m_mutex);
    m_confirmState.status = 3; // declined
    m_cond.wakeAll();
}

void ChatWorker::sendConfirmation(bool confirmed, bool yoloTurn) {
    QMutexLocker lock(&m_mutex);
    m_confirmState.status = confirmed ? 2 : 3;
    m_confirmState.yolo_turn = yoloTurn;
    m_cond.wakeAll();
}

void ChatWorker::run() {
    // C callback invoked from Rust for each LLM event
    auto callback = [](const char* json, void* data) {
        auto* self = static_cast<ChatWorker*>(data);
        if (self->m_cancelled) return;
        emit self->eventReceived(QString::fromUtf8(json));
    };

    QByteArray baseUrl = m_baseUrl.toUtf8();
    QByteArray apiKey = m_apiKey.toUtf8();
    QByteArray model = m_model.toUtf8();
    QByteArray msgs = m_messagesJson.toUtf8();
    QByteArray tc = m_toolConfirmation.toUtf8();

    bool ok = pengy_llm_chat_run(
        baseUrl.constData(), apiKey.constData(), model.constData(),
        msgs.constData(), tc.constData(),
        &m_confirmState,
        callback, this
    );

    if (!ok && !m_cancelled) {
        emit errorOccurred("LLM chat failed");
    }
    emit finished();
}
