/*
 * MicYou native plugin ABI (v1 / API v2).
 *
 * Standalone copy of MicYou's `micyou-plugin` ABI so a native plugin can be
 * built without the Rust crate. The struct layout and pointer order below MUST
 * match the host exactly (tauri-app/crates/micyou-plugin/src/abi.rs) — changing
 * them breaks ABI compatibility.
 *
 * Buffer convention (important): every "write a string out" host function is
 *   f(ctx, [key,] char *out, uint32_t *out_size)
 * where `*out_size` is IN/OUT: on entry it holds the buffer capacity in bytes,
 * on success the host writes the byte length (excluding NUL) back into it, and
 * on MPL_ERR_BUFFER_TOO_SMALL it writes the required size. There is NO separate
 * length argument — passing one shifts every following argument (crash).
 *
 * Build (no MSVC required, use zig):
 *   zig cc -target x86_64-windows-gnu -shared -O2 -o voicekey.dll voicekey.c -I.
 */
#ifndef MICYOU_PLUGIN_ABI_H
#define MICYOU_PLUGIN_ABI_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define MPL_ABI_VERSION 1u
#define MPL_API_VERSION 2u

#if defined(_WIN32)
#  define MPL_EXPORT __declspec(dllexport)
#else
#  define MPL_EXPORT __attribute__((visibility("default")))
#endif

typedef enum mpl_result {
    MPL_OK = 0,
    MPL_ERR_NOT_IMPLEMENTED = 1,
    MPL_ERR_INVALID_ARG = 2,
    MPL_ERR_RUNTIME = 3,
    MPL_ERR_BUFFER_TOO_SMALL = 4,
    MPL_ERR_PERMISSION = 5
} mpl_result_t;

typedef enum mpl_log_level {
    MPL_LOG_ERROR = 0,
    MPL_LOG_WARN = 1,
    MPL_LOG_INFO = 2,
    MPL_LOG_DEBUG = 3,
    MPL_LOG_TRACE = 4
} mpl_log_level_t;

typedef struct mpl_plugin_info {
    uint32_t    abi_version;
    uint32_t    api_version;
    const char *id;
    const char *version;
} mpl_plugin_info_t;

/*
 * Host API table. Every function takes `ctx` first and (except log) returns an
 * mpl_result_t. The order of these members is part of the ABI — do not reorder.
 * `ctx` sits deliberately after connected_devices.
 */
typedef struct mpl_host_api {
    void        (*log)(void *ctx, int level, const char *msg);
    mpl_result_t(*get_config)(void *ctx, const char *key, char *out, uint32_t *out_size);
    mpl_result_t(*set_config)(void *ctx, const char *key, const char *value_json);
    mpl_result_t(*emit_event)(void *ctx, const char *topic, const char *json_payload);
    mpl_result_t(*send_message)(void *ctx, const char *target_json, const uint8_t *payload, uint32_t payload_len);
    mpl_result_t(*audio_state)(void *ctx, char *out, uint32_t *out_size);
    mpl_result_t(*connected_devices)(void *ctx, char *out, uint32_t *out_size);
    void         *ctx;
    mpl_result_t(*play_sound)(void *ctx, const char *path);
    mpl_result_t(*plugin_dir)(void *ctx, char *out, uint32_t *out_size);
    mpl_result_t(*register_hotkey)(void *ctx, const char *shortcut, uint64_t *out_id);
    mpl_result_t(*open_window)(void *ctx, const char *panel_id);
    mpl_result_t(*fs_read)(void *ctx, const char *path, char *out, uint32_t *out_size);
    mpl_result_t(*fs_write)(void *ctx, const char *path, const char *content);
    mpl_result_t(*set_timeout)(void *ctx, uint64_t ms, const char *payload, uint64_t *out_id);
    mpl_result_t(*clear_timeout)(void *ctx, uint64_t id);
    mpl_result_t(*http_request)(void *ctx, const char *method, const char *url,
                                const char *headers_json, const char *body, uint64_t *out_id);
    mpl_result_t(*set_interval)(void *ctx, uint64_t ms, const char *payload, uint64_t *out_id);
    mpl_result_t(*clear_interval)(void *ctx, uint64_t id);
    mpl_result_t(*open_url)(void *ctx, const char *url);
    mpl_result_t(*notify)(void *ctx, const char *title, const char *body);
    mpl_result_t(*locale)(void *ctx, char *out, uint32_t *out_size);
    mpl_result_t(*host_info)(void *ctx, char *out, uint32_t *out_size);
    mpl_result_t(*clipboard_read)(void *ctx, char *out, uint32_t *out_size);
    mpl_result_t(*clipboard_write)(void *ctx, const char *text);
    mpl_result_t(*set_panel_icon)(void *ctx, const char *panel_id, const char *icon);
    mpl_result_t(*set_muted)(void *ctx, uint32_t muted);
    mpl_result_t(*get_muted)(void *ctx, uint32_t *out_muted);
    mpl_result_t(*set_monitoring)(void *ctx, uint32_t enabled);
    mpl_result_t(*get_monitoring)(void *ctx, uint32_t *out_enabled);
    mpl_result_t(*get_dsp_settings)(void *ctx, char *out, uint32_t *out_size);
    mpl_result_t(*set_dsp_settings)(void *ctx, const char *settings_json);
} mpl_host_api_t;

/* Required exports. */
MPL_EXPORT const mpl_plugin_info_t *micyou_plugin_info(void);
MPL_EXPORT mpl_result_t micyou_plugin_init(const mpl_host_api_t *host);
MPL_EXPORT void micyou_plugin_deinit(void);

#ifdef __cplusplus
}
#endif

#endif /* MICYOU_PLUGIN_ABI_H */
