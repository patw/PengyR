#pragma once

// C FFI declarations for pengy_core library
extern "C" {
    char* pengy_config_load();
    bool  pengy_config_save(const char* json);
    char* pengy_config_render(const char* template_str);
    void  pengy_config_set_dir(const char* path);

    char* pengy_models_cached_for(const char* base_url);
    bool  pengy_models_cache_save(const char* base_url, const char* models_json);

    char* pengy_chats_load();
    char* pengy_chat_index_load();
    char* pengy_chat_create(const char* title);
    bool  pengy_chat_delete(const char* id);
    bool  pengy_chat_save(const char* json);
    char* pengy_chat_get(const char* id);
    char* pengy_clean_messages(const char* json);
    char* pengy_messages_redact_last(const char* json);
    char* pengy_chat_add_usage(const char* chat_json, const char* usage_json);

    char* pengy_tasks_load();
    bool  pengy_tasks_save(const char* json);
    char* pengy_task_create(const char* title, const char* template_str);
    char* pengy_task_update(const char* id, const char* title, const char* template_str);
    bool  pengy_task_delete(const char* id);
    char* pengy_task_placeholders(const char* template_str);
    char* pengy_task_render(const char* template_str, const char* values_json);

    bool  pengy_tool_is_readonly(const char* name);
    void  pengy_tool_set_user_agent(const char* ua);
    void  pengy_tool_set_timeout(unsigned long long secs);
    void  pengy_tool_set_download_max_mb(unsigned long long mb);
    void  pengy_tool_set_output_max_chars(unsigned long long chars);

    char* pengy_image_preprocess(const char* path,
                                 unsigned int max_dimension,
                                 double max_mb,
                                 unsigned char quality);

    struct ConfirmState {
        int status;    // 0=idle, 1=pending, 2=confirmed, 3=declined
        bool yolo_turn;
    };

    struct SudoState {
        int status;           // 0=idle, 1=pending, 2=provided, 3=cancelled
        char password[256];
    };

    struct QuestionState {
        int status;                // 0=idle, 1=pending, 2=answered, 3=cancelled
        char questions_json[16384];
        char answers_json[4096];
    };

    typedef void (*EventFn)(const char* event_json, void* userdata);

    // Opaque per-run tool context handle (Rust Arc<ToolContext>).  Create one
    // per worker so a Stop kills only that run's subprocesses and its sudo
    // provider is never clobbered by another tab.
    typedef struct PengyRun PengyRun;
    PengyRun* pengy_run_new(void);
    void      pengy_run_free(PengyRun* run);

    bool pengy_llm_chat_run(
        const char* base_url,
        const char* api_key,
        const char* model,
        const char* messages_json,
        const char* tool_confirmation,
        const char* reasoning_effort,
        bool preserve_reasoning,
        ConfirmState* confirm_state,
        SudoState* sudo_state,
        QuestionState* question_state,
        EventFn on_event,
        void* userdata,
        PengyRun* run
    );

    void pengy_llm_cancel(PengyRun* run);
    void pengy_free(char* s);
}
