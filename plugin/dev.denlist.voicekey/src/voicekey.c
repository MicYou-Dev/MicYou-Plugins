/*
 * VoiceKey — MicYou native plugin.
 *
 * Watches the host's audio input level (polled via a 50 ms set_interval) and,
 * when speech is detected, presses and holds a configurable key combo with
 * Win32 SendInput; it releases on silence. This lets push-to-talk voice input
 * (WeChat / Sogou / etc., default Ctrl+Win) start hands-free when you talk into
 * your phone through MicYou.
 *
 * kind=utility so it stays out of the DSP chain and never touches audio bytes.
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <ctype.h>

#include "micyou_plugin_abi.h"

static const char *const PLUGIN_ID      = "dev.denlist.voicekey";
static const char *const PLUGIN_VERSION = "1.1.1";

/* ── host table (copied by value in init, as the ABI requires) ───────────── */
static mpl_host_api_t g_host;
static int g_inited = 0;

/* ── config ──────────────────────────────────────────────────────────────── */
typedef struct {
    int      enabled;
    char     keys[128];
    float    threshold;
    uint32_t silence_ms;      /* how long silent before releasing            */
    uint32_t min_hold_ms;     /* keep held at least this long (debounce)      */
    uint32_t max_hold_ms;     /* safety cap: force release after this long    */
    uint32_t start_delay_ms;  /* debounce before pressing (ignore blips)     */
    uint32_t cooldown_ms;     /* ignore new speech for this long after release*/
    int      only_when_streaming;
    int      inject_scan;     /* 0 = VK (default), 1 = scancode */
} config_t;

static config_t g_cfg;

/* ── runtime state ───────────────────────────────────────────────────────── */
typedef struct {
    int      pressed;
    int      have_start;
    DWORD    press_tick;
    int      started;         /* 1 once we've actually pressed for this utterance */
    DWORD    last_release_tick; /* for cooldown */
    float    last_level;
    int      streaming;
    int      muted;
} state_t;

static state_t g_st;

/* ── tiny JSON value readers ─────────────────────────────────────────────── *
 * The host's get_config returns the raw JSON encoding of the value (e.g.
 * `"ctrl+win"`, `0.015`, `true`), NOT an object. So we parse the bare value. */
static int jval_bool(const char *s, int dflt) {
    if (!s) return dflt;
    while (*s && isspace((unsigned char)*s)) s++;
    if (s[0] == 't' || s[0] == 'T') return 1;
    if (s[0] == 'f' || s[0] == 'F') return 0;
    return dflt;
}

static double jval_num(const char *s, double dflt) {
    if (!s) return dflt;
    while (*s && isspace((unsigned char)*s)) s++;
    char *end = NULL;
    double v = strtod(s, &end);
    if (end == s) return dflt;
    return v;
}

/* copy a JSON string value (handles simple escapes) into out */
static void jval_str(const char *s, char *out, size_t outlen, const char *dflt) {
    if (!s || !out || outlen == 0) return;
    while (*s && isspace((unsigned char)*s)) s++;
    if (*s == '"') {
        s++;
        size_t i = 0;
        while (*s && *s != '"' && i + 1 < outlen) {
            if (*s == '\\' && s[1]) {
                s++;
                switch (*s) {
                    case 'n': out[i++] = '\n'; break;
                    case 't': out[i++] = '\t'; break;
                    case 'r': out[i++] = '\r'; break;
                    case '\\': out[i++] = '\\'; break;
                    case '"': out[i++] = '"'; break;
                    default: out[i++] = *s; break;
                }
            } else {
                out[i++] = *s;
            }
            s++;
        }
        out[i] = '\0';
        if (i > 0) return;
    }
    if (dflt) { strncpy(out, dflt, outlen - 1); out[outlen - 1] = '\0'; }
}

/* read a config key and parse it as a bare JSON value */
static int cfg_read_str(const char *key, char *out, size_t outlen, const char *dflt) {
    char buf[512];
    uint32_t out_size = (uint32_t)sizeof(buf);
    if (!g_host.get_config) return 0;
    buf[0] = '\0';
    mpl_result_t r = g_host.get_config(g_host.ctx, key, buf, &out_size);
    if (r != MPL_OK) { if (dflt) { strncpy(out, dflt, outlen - 1); out[outlen - 1] = 0; } return 0; }
    if (out_size < sizeof(buf)) buf[out_size] = '\0'; else buf[sizeof(buf) - 1] = '\0';
    jval_str(buf, out, outlen, dflt);
    return 1;
}

static double cfg_read_num(const char *key, double dflt) {
    char buf[128];
    uint32_t out_size = (uint32_t)sizeof(buf);
    if (!g_host.get_config) return dflt;
    buf[0] = '\0';
    mpl_result_t r = g_host.get_config(g_host.ctx, key, buf, &out_size);
    if (r != MPL_OK) return dflt;
    if (out_size < sizeof(buf)) buf[out_size] = '\0'; else buf[sizeof(buf) - 1] = '\0';
    return jval_num(buf, dflt);
}

static int cfg_read_bool(const char *key, int dflt) {
    char buf[64];
    uint32_t out_size = (uint32_t)sizeof(buf);
    if (!g_host.get_config) return dflt;
    buf[0] = '\0';
    mpl_result_t r = g_host.get_config(g_host.ctx, key, buf, &out_size);
    if (r != MPL_OK) return dflt;
    if (out_size < sizeof(buf)) buf[out_size] = '\0'; else buf[sizeof(buf) - 1] = '\0';
    return jval_bool(buf, dflt);
}

static void load_config(void) {
    g_cfg.enabled             = cfg_read_bool("enabled", 1);
    cfg_read_str("keys", g_cfg.keys, sizeof(g_cfg.keys), "ctrl+win");
    g_cfg.threshold           = (float)cfg_read_num("threshold", 0.015);
    g_cfg.silence_ms          = (uint32_t)cfg_read_num("silenceMs", 1000);
    g_cfg.min_hold_ms         = (uint32_t)cfg_read_num("minHoldMs", 700);
    g_cfg.max_hold_ms         = (uint32_t)cfg_read_num("maxHoldMs", 60000);
    g_cfg.start_delay_ms      = (uint32_t)cfg_read_num("startDelayMs", 120);
    g_cfg.cooldown_ms         = (uint32_t)cfg_read_num("cooldownMs", 800);
    g_cfg.only_when_streaming = cfg_read_bool("onlyWhenStreaming", 1);
    char inj[32]; cfg_read_str("injectMode", inj, sizeof(inj), "vk");
    g_cfg.inject_scan = (strcmp(inj, "scan") == 0);
}

/* ── key combo parsing ───────────────────────────────────────────────────── *
 * Parse "ctrl+win" style strings into an array of VKs. Modifiers first, then
 * the main key (if any). Unknown tokens are ignored. */
static int key_name_to_vk(const char *tok) {
    if (!strcmp(tok, "ctrl") || !strcmp(tok, "control")) return VK_CONTROL;
    if (!strcmp(tok, "shift")) return VK_SHIFT;
    if (!strcmp(tok, "alt") || !strcmp(tok, "menu")) return VK_MENU;
    if (!strcmp(tok, "win") || !strcmp(tok, "cmd") || !strcmp(tok, "meta")) return VK_LWIN;
    if (!strcmp(tok, "space")) return VK_SPACE;
    if (!strcmp(tok, "enter") || !strcmp(tok, "return")) return VK_RETURN;
    if (!strcmp(tok, "tab")) return VK_TAB;
    if (!strcmp(tok, "esc") || !strcmp(tok, "escape")) return VK_ESCAPE;
    if (!strcmp(tok, "backspace")) return VK_BACK;
    if (!strcmp(tok, "delete") || !strcmp(tok, "del")) return VK_DELETE;
    if (tok[0] == 'f' && isdigit((unsigned char)tok[1])) {
        int n = atoi(tok + 1);
        if (n >= 1 && n <= 24) return VK_F1 + (n - 1);
    }
    if (tok[0] == 'v' && tok[1] == 'k' && isdigit((unsigned char)tok[2]))
        return atoi(tok + 2);
    if ((tok[0] >= 'a' && tok[0] <= 'z') && tok[1] == '\0')
        return (int)(tok[0] - 'a' + 'A');
    if ((tok[0] >= '0' && tok[0] <= '9') && tok[1] == '\0')
        return (int)tok[0];
    return 0;
}

/* split keys string into vks; returns count */
static int parse_keys(const char *keys, int *vks, int max) {
    int n = 0;
    char tmp[128];
    strncpy(tmp, keys, sizeof(tmp) - 1); tmp[sizeof(tmp) - 1] = '\0';
    char *p = tmp;
    while (*p && n < max) {
        char *plus = strchr(p, '+');
        if (plus) *plus = '\0';
        /* trim */
        while (*p && isspace((unsigned char)*p)) p++;
        char *e = p + strlen(p);
        while (e > p && isspace((unsigned char)e[-1])) *--e = '\0';
        if (*p) {
            int vk = key_name_to_vk(p);
            if (vk) vks[n++] = vk;
        }
        if (!plus) break;
        p = plus + 1;
    }
    return n;
}

/* ── SendInput press / release ───────────────────────────────────────────── */
static int g_pressed_keys[16];
static int g_pressed_count = 0;
static int g_pressed_scan = 0;

static void send_key(int vk, int scan, int up) {
    INPUT in;
    ZeroMemory(&in, sizeof(in));
    in.type = INPUT_KEYBOARD;
    in.ki.wVk = (WORD)vk;
    in.ki.wScan = (WORD)scan;
    in.ki.dwFlags = up ? KEYEVENTF_KEYUP : 0;
    if (scan) in.ki.dwFlags |= KEYEVENTF_SCANCODE;
    SendInput(1, &in, sizeof(INPUT));
}

static void do_press(void) {
    int vks[16];
    int n = parse_keys(g_cfg.keys, vks, 16);
    if (n <= 0) return;
    g_pressed_count = 0;
    g_pressed_scan = g_cfg.inject_scan;
    for (int i = 0; i < n; i++) {
        /* scancode mode: MapVirtualKey to scan code */
        int scan = g_cfg.inject_scan ? (int)MapVirtualKeyW((UINT)vks[i], MAPVK_VK_TO_VSC) : 0;
        send_key(vks[i], scan, 0);
        g_pressed_keys[g_pressed_count++] = vks[i];
    }
    g_st.pressed = 1;
    g_st.started = 1;
    g_st.press_tick = GetTickCount();
    if (g_host.log)
        g_host.log(g_host.ctx, MPL_LOG_INFO, "[VoiceKey] 检测到语音，已按下按键");
}

static void do_release(const char *reason) {
    if (!g_st.pressed) return;
    for (int i = g_pressed_count - 1; i >= 0; i--) {
        int vk = g_pressed_keys[i];
        int scan = g_pressed_scan ? (int)MapVirtualKeyW((UINT)vk, MAPVK_VK_TO_VSC) : 0;
        send_key(vk, scan, 1);
    }
    g_pressed_count = 0;
    g_st.pressed = 0;
    g_st.started = 0;
    g_st.have_start = 0;
    g_st.last_release_tick = GetTickCount();
    if (g_host.log && reason) {
        char msg[160];
        _snprintf_s(msg, sizeof(msg), _TRUNCATE, "[VoiceKey] 已松开按键（%s）", reason);
        g_host.log(g_host.ctx, MPL_LOG_INFO, msg);
    }
}

/* ── probe: publish state for the panel ──────────────────────────────────── */
static void write_probe(void) {
    char json[320];
    _snprintf_s(json, sizeof(json), _TRUNCATE,
        "{\"ok\":true,\"level\":%.6f,\"streaming\":%s,\"muted\":%s,"
        "\"state\":\"%s\",\"pressed\":%s,\"keys\":\"%s\"}",
        g_st.last_level,
        g_st.streaming ? "true" : "false",
        g_st.muted ? "true" : "false",
        g_st.pressed ? "holding" : "idle",
        g_st.pressed ? "true" : "false",
        g_cfg.keys);
    if (g_host.set_config)
        g_host.set_config(g_host.ctx, "lastProbe", json);
}

/* ── audio state poll (the VAD core) ─────────────────────────────────────── */
static void poll_once(void) {
    static DWORD s_last_cfg = 0;
    DWORD now = GetTickCount();
    if (now - s_last_cfg > 2000) { load_config(); s_last_cfg = now; }

    if (!g_cfg.enabled) { if (g_st.pressed) do_release("disabled"); return; }

    char buf[512];
    uint32_t out_len = (uint32_t)sizeof(buf);
    buf[0] = '\0';
    if (!g_host.audio_state) return;
    mpl_result_t r = g_host.audio_state(g_host.ctx, buf, &out_len);
    if (r != MPL_OK) return;
    if (out_len < sizeof(buf)) buf[out_len] = '\0'; else buf[sizeof(buf) - 1] = '\0';

    /* the snapshot is a JSON object; extract fields loosely */
    float level = 0.0f;
    int streaming = 0, muted = 0;
    {
        const char *p = strstr(buf, "\"inputLevel\"");
        if (p) level = (float)jval_num(p + 13, 0.0);
        p = strstr(buf, "\"streaming\"");
        if (p) streaming = jval_bool(p + 12, 0);   /* skip 11 chars + ':' */
        p = strstr(buf, "\"muted\"");
        if (p) muted = jval_bool(p + 7, 0);
    }
    g_st.last_level = level;
    g_st.streaming = streaming;
    g_st.muted = muted;

    int active = (level >= g_cfg.threshold);
    if (g_cfg.only_when_streaming && !streaming) active = 0;
    if (muted) active = 0;

    /* silence tracking shared across frames */
    static DWORD s_below_since = 0;
    if (active) s_below_since = 0;

    /* cooldown: after a release, ignore new speech briefly so trailing noise
     * doesn't immediately re-trigger. */
    int in_cooldown = (g_cfg.cooldown_ms > 0) &&
                      (now - g_st.last_release_tick < g_cfg.cooldown_ms);

    if (active && !in_cooldown && !g_st.pressed) {
        if (!g_st.have_start) { g_st.have_start = 1; g_st.press_tick = now; }
        if (now - g_st.press_tick >= g_cfg.start_delay_ms) {
            do_press();
        }
    } else if (!active && g_st.pressed) {
        if (!s_below_since) s_below_since = now;
        DWORD held = now - s_below_since;
        /* release only once silent long enough AND held at least min_hold_ms */
        if (held >= g_cfg.silence_ms &&
            (now - g_st.press_tick) >= g_cfg.min_hold_ms) {
            do_release("静音超时");
            s_below_since = 0;
        }
    }

    /* cancel a pending start if the blip ended before start_delay elapsed */
    if (!active && g_st.have_start && !g_st.pressed) {
        g_st.have_start = 0;
    }

    /* safety: max hold */
    if (g_st.pressed && g_cfg.max_hold_ms > 0 &&
        (now - g_st.press_tick) >= g_cfg.max_hold_ms) {
        do_release("超过最长按住时间");
    }

    write_probe();
}

/* ── interval / message handling ─────────────────────────────────────────── */
static uint64_t g_interval_id = 0;

static void on_interval_tick(void) { poll_once(); }

MPL_EXPORT mpl_result_t micyou_plugin_handle_message(const char *source, const char *topic,
                                                     const uint8_t *payload, uint32_t payload_len) {
    (void)source;
    if (!topic) return MPL_ERR_INVALID_ARG;

    if (!strncmp(topic, "ui:", 3)) {
        const char *action = topic + 3;
        if (!strcmp(action, "test")) {
            /* trigger one press+release cycle for verification */
            do_press();
            Sleep(300);
            do_release("面板测试");
            return MPL_OK;
        }
        if (!strcmp(action, "probe")) {
            poll_once();
            return MPL_OK;
        }
    }
    if (!strcmp(topic, "interval:tick")) {
        on_interval_tick();
        return MPL_OK;
    }
    if (!strcmp(topic, "config:changed")) {
        load_config();
        return MPL_OK;
    }
    if (!strcmp(topic, "device_disconnected")) {
        if (g_st.pressed) do_release("device_disconnected");
        return MPL_OK;
    }
    (void)payload; (void)payload_len;
    return MPL_ERR_NOT_IMPLEMENTED;
}

/* ── required exports ────────────────────────────────────────────────────── */
MPL_EXPORT const mpl_plugin_info_t *micyou_plugin_info(void) {
    static const mpl_plugin_info_t info = {
        MPL_ABI_VERSION, MPL_API_VERSION, PLUGIN_ID, PLUGIN_VERSION
    };
    return &info;
}

MPL_EXPORT mpl_result_t micyou_plugin_init(const mpl_host_api_t *host) {
    if (!host) return MPL_ERR_INVALID_ARG;
    g_host = *host;              /* copy the whole table by value */
    g_inited = 1;

    /* defaults */
    ZeroMemory(&g_st, sizeof(g_st));
    strcpy_s(g_cfg.keys, sizeof(g_cfg.keys), "ctrl+win");
    g_cfg.threshold      = 0.015f;
    g_cfg.silence_ms     = 1000;
    g_cfg.min_hold_ms    = 700;
    g_cfg.max_hold_ms    = 60000;
    g_cfg.start_delay_ms = 120;
    g_cfg.cooldown_ms    = 800;
    g_cfg.enabled        = 1;
    g_cfg.only_when_streaming = 1;
    g_cfg.inject_scan    = 0;

    load_config();

    if (g_host.log) {
        char msg[220];
        _snprintf_s(msg, sizeof(msg), _TRUNCATE,
            "VoiceKey 已启动：绑定=%s 模式=按住说话 阈值=%.3f 静音判定=%ums",
            g_cfg.keys, g_cfg.threshold, g_cfg.silence_ms);
        g_host.log(g_host.ctx, MPL_LOG_INFO, msg);
    }

    /* start the 50 ms poll */
    if (g_host.set_interval) {
        g_host.set_interval(g_host.ctx, 50, "poll", &g_interval_id);
    }
    return MPL_OK;
}

MPL_EXPORT void micyou_plugin_deinit(void) {
    if (g_st.pressed) do_release("deinit");
    if (g_host.clear_interval && g_interval_id) {
        g_host.clear_interval(g_host.ctx, g_interval_id);
        g_interval_id = 0;
    }
    if (g_host.log) g_host.log(g_host.ctx, MPL_LOG_INFO, "VoiceKey 已停止");
    ZeroMemory(&g_host, sizeof(g_host));
    g_inited = 0;
}
