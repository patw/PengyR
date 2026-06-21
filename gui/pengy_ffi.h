#pragma once

// C FFI declarations for pengy_core library
extern "C" {
    char* pengy_config_load();
    bool  pengy_config_save(const char* json);
    char* pengy_config_render(const char* template_str);

    char* pengy_chats_load();
    char* pengy_chat_create(const char* title);
    bool  pengy_chat_delete(const char* id);
    bool  pengy_chat_save(const char* json);
    char* pengy_chat_get(const char* id);
    char* pengy_clean_messages(const char* json);

    bool  pengy_tool_is_readonly(const char* name);
    void  pengy_tool_set_user_agent(const char* ua);
    void  pengy_tool_set_timeout(unsigned long long secs);

    struct ConfirmState {
        int status;    // 0=idle, 1=pending, 2=confirmed, 3=declined
        bool yolo_turn;
    };

    struct SudoState {
        int status;           // 0=idle, 1=pending, 2=provided, 3=cancelled
        char password[256];
    };

    typedef void (*EventFn)(const char* event_json, void* userdata);

    bool pengy_llm_chat_run(
        const char* base_url,
        const char* api_key,
        const char* model,
        const char* messages_json,
        const char* tool_confirmation,
        ConfirmState* confirm_state,
        SudoState* sudo_state,
        EventFn on_event,
        void* userdata
    );

    void pengy_llm_cancel(bool* cancel_flag);
    void pengy_free(char* s);
}
