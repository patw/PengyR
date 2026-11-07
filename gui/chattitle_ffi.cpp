// ChatTitle backed by the Rust core (see chattitle.h).
#include "chattitle.h"
#include "pengy_ffi.h"

#include <QJsonArray>
#include <QJsonDocument>

namespace {

QJsonDocument takeJson(char* raw) {
    if (!raw) return {};
    QJsonDocument doc = QJsonDocument::fromJson(QByteArray(raw));
    pengy_free(raw);
    return doc;
}

}  // namespace

namespace ChatTitle {

QJsonObject generate(const QJsonObject& chat) {
    QByteArray json = QJsonDocument(chat).toJson(QJsonDocument::Compact);
    QJsonObject out = takeJson(pengy_title_generate(json.constData())).object();
    if (out.isEmpty()) out = {{"error", "Title generation failed."}, {"unusable", false}};
    return out;
}

QJsonObject bulkStep(const QString& chatId, bool redo) {
    QJsonObject out = takeJson(pengy_title_bulk_step(chatId.toUtf8().constData(), redo)).object();
    if (out.isEmpty()) out = {{"status", "failed"}, {"error", "Title generation failed."}};
    return out;
}

QStringList bulkTargets(bool redo) {
    QStringList ids;
    for (const QJsonValue& v : takeJson(pengy_title_bulk_targets(redo)).array())
        ids << v.toString();
    return ids;
}

}  // namespace ChatTitle
