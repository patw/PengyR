#ifndef PENGY_CHAT_LABELS_H
#define PENGY_CHAT_LABELS_H

#include <QString>

// Chat role label defaults, shared by the window (which derives the labels it
// renders) and the settings dialog (which shows them prefilled).
//
// A label is the whole string the transcript draws: the view appends no glyph of
// its own, so deleting the emoji is a settings edit, not a code edit. {username}
// is the login name, so there is no separate display-name setting -- a name goes
// in the label itself. {hostname} becomes ' (host)' when the node is named.
//
// Kept in sync with configChatIdentity() in PengyCPP and config::chat_identity()
// in PengyR's Rust core.

inline QString chatUserGlyph() { return QString::fromUtf8("\xf0\x9f\xa7\x91"); }
inline QString chatAssistantGlyph() { return QString::fromUtf8("\xf0\x9f\xa4\x96"); }

inline QString chatDefaultUserLabel() {
    // "You" is a literal, not {username}: the field is the whole string, and the
    // login name is reached by writing {username} into it.
    return QStringLiteral("You ") + chatUserGlyph();
}

inline QString chatDefaultAssistantLabel() {
    return QStringLiteral("Pengy{hostname} ") + chatAssistantGlyph();
}

/// A blank (or whitespace-only) field restores the shipped default, so a cleared
/// settings box can never render a label-less transcript.
inline QString chatLabelField(const QString& configured, const QString& fallback) {
    const QString trimmed = configured.trimmed();
    return trimmed.isEmpty() ? fallback : trimmed;
}

/// Saving the default text stores empty, so a future change to the shipped default
/// is not pinned by a config file that happens to repeat it.
inline QString chatLabelToStore(const QString& text, const QString& fallback) {
    const QString trimmed = text.trimmed();
    return (trimmed.isEmpty() || trimmed == fallback) ? QString() : trimmed;
}

#endif
