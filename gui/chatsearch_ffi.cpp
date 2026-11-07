// ChatSearch backed by the Rust core (see chatsearch.h and src/chat_search.rs).
#include "chatsearch.h"
#include "pengy_ffi.h"

#include <QJsonDocument>

namespace {

bool forwardEvent(const char* eventJson, void* userdata) {
    const auto& onEvent = *static_cast<const std::function<bool(const QJsonObject&)>*>(userdata);
    return onEvent(QJsonDocument::fromJson(QByteArray(eventJson)).object());
}

}  // namespace

namespace ChatSearch {

void run(const QString& query, int limit, const std::function<bool(const QJsonObject&)>& onEvent) {
    pengy_search_chats(query.toUtf8().constData(), size_t(limit > 0 ? limit : 0), forwardEvent,
                       const_cast<void*>(static_cast<const void*>(&onEvent)));
}

}  // namespace ChatSearch
