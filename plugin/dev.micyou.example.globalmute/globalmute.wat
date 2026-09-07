;; MicYou WASM example: global mute (全局静音)
;;
;; 通过「加入音频处理链」的方式实现全局静音：manifest 声明 kind=dsp 且
;; dsp.first=true，节点被宿主插到处理链最前（AEC 之前）；process 在静音
;; 状态下把整帧样本清零，下游节点与虚拟麦克风/扬声器输出收到的都是全零
;; 数据，从而对全部输出实现真正的全局静音。
;;
;; 交互：
;;   - 全局快捷键（init 时 register_hotkey 注册，默认 ctrl+shift+m）按下
;;     后宿主经总线投递 topic hotkey:<id>，payload {"shortcut":"..."}，
;;     WASM 插件经 handle_message 收到该 JSON → 切换静音并 set_config 持久化
;;   - 面板按钮经 trigger → handle_message 收到 {"action":"toggle|mute|unmute"}
;;   - 宿主设置页/面板录入 config 热更新 → handle_message 收到
;;     {"key":"muted","value":true} 或 {"key":"shortcut","value":"..."}
;;     - muted：直接更新内存状态
;;     - shortcut：宿主没有注销热键的 API，插件重新读取配置并再次
;;       register_hotkey 让新快捷键立即生效；旧热键在宿主端会残留，
;;       插件用 payload 里的 shortcut 与当前生效值比对来忽略残留触发
;;
;; Imports (module "micyou"): log / get_config / set_config / register_hotkey /
;;                            set_panel_icon
;; Exports: memory / alloc / dealloc / api_version / init / process / handle_message / deinit
;;
;; Memory layout (static):
;;   0x100 "muted\0"            config key
;;   0x108 "shortcut\0"         config key
;;   0x118 $MUTED (i32)         静音状态 0=直播 1=静音
;;   0x120 "ctrl+shift+m\0"     默认快捷键（config 无 shortcut 时回退）
;;   0x140 "\"action\":"        面板 trigger needle
;;   0x149 "\"key\":"           config:changed needle
;;   0x150 "\"shortcut\":"      hotkey 按下 needle
;;   0x160 "\"muted\""          muted 键 needle（config:changed 中 key 后跟逗号）
;;   0x168 "\"toggle\""         action 值
;;   0x170 "\"mute\""           action 值
;;   0x176 "\"unmute\""         action 值
;;   0x180 "global mute ready"  日志
;;   0x1A0 "mute\0"             panel id
;;   0x1A8 🔇 (F0 9F 94 87)     面板图标
;;   0x1B0 "true\0" / 0x1B6 "false\0"
;;   0x1C0 "hotkey re-registered" 日志
;;   0x1D8 "stale hotkey ignored"  日志
;;   0x2000 当前生效快捷键 buffer（64 字节）
;;   0x2100 临时 buffer（payload 快捷键提取 / init 校验，64 字节）
;;   heap bump 从 0x2200 开始

(module
  (import "micyou" "log" (func $log (param i32 i32)))
  (import "micyou" "get_config" (func $get_config (param i32) (result i32)))
  (import "micyou" "set_config" (func $set_config (param i32 i32) (result i32)))
  (import "micyou" "register_hotkey" (func $register_hotkey (param i32) (result i64)))
  (import "micyou" "set_panel_icon" (func $set_panel_icon (param i32 i32)))

  (memory (export "memory") 4)

  ;; ---------- static data ----------
  (data (i32.const 0x100) "muted\00")
  (data (i32.const 0x108) "shortcut\00")
  (data (i32.const 0x120) "ctrl+shift+m\00")
  (data (i32.const 0x140) "\"action\":")
  (data (i32.const 0x149) "\"key\":")
  (data (i32.const 0x150) "\"shortcut\":")
  (data (i32.const 0x160) "\"muted\"")
  (data (i32.const 0x168) "\"toggle\"")
  (data (i32.const 0x170) "\"mute\"")
  (data (i32.const 0x176) "\"unmute\"")
  (data (i32.const 0x180) "global mute ready\00")
  (data (i32.const 0x1A0) "mute\00")
  (data (i32.const 0x1A8) "\F0\9F\94\87\00")
  (data (i32.const 0x1B0) "true\00")
  (data (i32.const 0x1B6) "false\00")
  (data (i32.const 0x1C0) "hotkey re-registered\00")
  (data (i32.const 0x1D8) "stale hotkey ignored\00")
  (data (i32.const 0x2000) "ctrl+shift+m\00")

  ;; ---------- bump allocator ----------
  (global $heap (mut i32) (i32.const 0x2200))
  (func (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (i32.store (local.get $p) (local.get $n))
    (global.set $heap (i32.add (global.get $heap) (i32.add (local.get $n) (i32.const 8))))
    (i32.add (local.get $p) (i32.const 8)))
  (func (export "dealloc") (param $p i32) (param $n i32))

  (func (export "api_version") (result i32) (i32.const 1))

  ;; ---------- helpers ----------
  ;; does [h..h+hlen) contain needle [n..n+nlen)? returns 0/1
  (func $contains (param $h i32) (param $hlen i32) (param $n i32) (param $nlen i32) (result i32)
    (local $i i32) (local $j i32)
    (block $out (result i32)
      (loop $l
        (br_if $out (i32.const 0) (i32.gt_u (i32.add (local.get $i) (local.get $nlen)) (local.get $hlen)))
        (local.set $j (i32.const 0))
        (if (i32.eqz (block $match (result i32)
              (loop $m
                (br_if $match (i32.const 1) (i32.ge_u (local.get $j) (local.get $nlen)))
                (br_if $match (i32.const 0)
                  (i32.ne
                    (i32.load8_u (i32.add (local.get $h) (i32.add (local.get $i) (local.get $j))))
                    (i32.load8_u (i32.add (local.get $n) (local.get $j)))))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $m))
              (i32.const 0)))
          (then (nop))
          (else (br $out (i32.const 1))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l))
      (i32.const 0)))

  ;; copy JSON string value at src (e.g. "ctrl+shift+m", quoted) into dst,
  ;; stripping the surrounding double quotes; NUL-terminates dst
  (func $copy_cstr (param $src i32) (param $dst i32)
    (local $i i32) (local $j i32) (local $c i32)
    (local.set $i (i32.add (local.get $src) (i32.const 1))) ;; skip opening quote
    (block $done
      (loop $l
        (local.set $c (i32.load8_u (i32.add (local.get $i) (local.get $j))))
        (br_if $done (i32.eqz (local.get $c)))
        (br_if $done (i32.eq (local.get $c) (i32.const 34))) ;; closing '"'
        (i32.store8 (i32.add (local.get $dst) (local.get $j)) (local.get $c))
        (local.set $j (i32.add (local.get $j) (i32.const 1)))
        (br $l)))
    (i32.store8 (i32.add (local.get $dst) (local.get $j)) (i32.const 0)))

  ;; find first offset of needle [n..n+nlen) in [h..h+hlen); -1 if absent
  (func $find (param $h i32) (param $hlen i32) (param $n i32) (param $nlen i32) (result i32)
    (local $i i32) (local $j i32)
    (block $out (result i32)
      (loop $l
        (br_if $out (i32.const -1) (i32.gt_u (i32.add (local.get $i) (local.get $nlen)) (local.get $hlen)))
        (local.set $j (i32.const 0))
        (if (i32.eqz (block $match (result i32)
              (loop $m
                (br_if $match (i32.const 1) (i32.ge_u (local.get $j) (local.get $nlen)))
                (br_if $match (i32.const 0)
                  (i32.ne
                    (i32.load8_u (i32.add (local.get $h) (i32.add (local.get $i) (local.get $j))))
                    (i32.load8_u (i32.add (local.get $n) (local.get $j)))))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $m))
              (i32.const 0)))
          (then (nop))
          (else (br $out (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l))
      (i32.const -1)))

  ;; copy the JSON string value right after needle at offset into dst
  ;; (strips the surrounding double quotes); NUL-terminates dst
  (func $extract_val (param $h i32) (param $hlen i32) (param $off i32) (param $nlen i32) (param $dst i32)
    (local $i i32) (local $j i32) (local $c i32)
    (local.set $i (i32.add (local.get $off) (local.get $nlen)))
    (if (i32.eq (i32.load8_u (i32.add (local.get $h) (local.get $i))) (i32.const 34))
      (then (local.set $i (i32.add (local.get $i) (i32.const 1)))))
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $hlen)))
        (local.set $c (i32.load8_u (i32.add (local.get $h) (local.get $i))))
        (br_if $done (i32.eq (local.get $c) (i32.const 34)))
        (br_if $done (i32.eqz (local.get $c)))
        (i32.store8 (i32.add (local.get $dst) (local.get $j)) (local.get $c))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (local.set $j (i32.add (local.get $j) (i32.const 1)))
        (br $l)))
    (i32.store8 (i32.add (local.get $dst) (local.get $j)) (i32.const 0)))

  ;; bytewise compare two NUL-terminated strings; returns 0/1
  (func $eq_cstr (param $a i32) (param $b i32) (result i32)
    (local $i i32) (local $ca i32) (local $cb i32)
    (block $done
      (loop $l
        (local.set $ca (i32.load8_u (i32.add (local.get $a) (local.get $i))))
        (local.set $cb (i32.load8_u (i32.add (local.get $b) (local.get $i))))
        (br_if $done (i32.ne (local.get $ca) (local.get $cb)))
        (br_if $done (i32.eqz (local.get $ca)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i32.eq (local.get $ca) (local.get $cb)))

  ;; shortcut char whitelist: a-z 0-9 '+'
  (func $is_ok_char (param $c i32) (result i32)
    (i32.or
      (i32.and (i32.ge_u (local.get $c) (i32.const 97)) (i32.le_u (local.get $c) (i32.const 122)))
      (i32.or
        (i32.and (i32.ge_u (local.get $c) (i32.const 48)) (i32.le_u (local.get $c) (i32.const 57)))
        (i32.eq (local.get $c) (i32.const 43)))))

  ;; validate a NUL-terminated shortcut: non-empty, all chars allowed
  (func $valid_shortcut (param $p i32) (result i32)
    (local $i i32) (local $c i32) (local $n i32) (local $ok i32)
    (local.set $ok (i32.const 1))
    (block $done
      (loop $l
        (local.set $c (i32.load8_u (i32.add (local.get $p) (local.get $i))))
        (br_if $done (i32.eqz (local.get $c)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (if (call $is_ok_char (local.get $c))
          (then (nop))
          (else (local.set $ok (i32.const 0)) (br $done)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i32.and (local.get $ok) (i32.gt_u (local.get $n) (i32.const 0))))

  ;; copy NUL-terminated string src -> dst
  (func $strcpy (param $src i32) (param $dst i32)
    (local $i i32) (local $c i32)
    (block $done
      (loop $l
        (local.set $c (i32.load8_u (i32.add (local.get $src) (local.get $i))))
        (i32.store8 (i32.add (local.get $dst) (local.get $i)) (local.get $c))
        (br_if $done (i32.eqz (local.get $c)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l))))

  ;; ---------- mute state ----------
  ;; update memory only (config:changed path: host already persisted)
  (func $apply_mute (param $v i32)
    (i32.store (i32.const 0x118) (local.get $v)))

  ;; update memory and persist to config
  (func $set_mute (param $v i32)
    (i32.store (i32.const 0x118) (local.get $v))
    (if (local.get $v)
      (then (drop (call $set_config (i32.const 0x100) (i32.const 0x1B0))))
      (else (drop (call $set_config (i32.const 0x100) (i32.const 0x1B6))))))

  (func $toggle_mute
    (call $set_mute (i32.xor (i32.load (i32.const 0x118)) (i32.const 1))))

  ;; ---------- init ----------
  (func (export "init") (result i32)
    (local $ptr i32) (local $c i32)
    ;; restore persisted mute state
    (local.set $ptr (call $get_config (i32.const 0x100)))
    (if (i32.gt_s (local.get $ptr) (i32.const 0))
      (then
        (local.set $c (i32.load8_u (local.get $ptr)))
        (if (i32.eq (local.get $c) (i32.const 116)) ;; 't' -> true
          (then (i32.store (i32.const 0x118) (i32.const 1)))
          (else (i32.store (i32.const 0x118) (i32.const 0))))))
    ;; configured shortcut: copy to temp, validate, only adopt if valid
    ;; (0x2000 is preloaded with the default, so an invalid value keeps default)
    (local.set $ptr (call $get_config (i32.const 0x108)))
    (if (i32.gt_s (local.get $ptr) (i32.const 0))
      (then
        (call $copy_cstr (local.get $ptr) (i32.const 0x2100))
        (if (call $valid_shortcut (i32.const 0x2100))
          (then (call $strcpy (i32.const 0x2100) (i32.const 0x2000))))))
    ;; register global hotkey (best-effort: host returns 0 on failure and init
    ;; must not fail, otherwise the plugin cannot be re-enabled from disabled)
    (drop (call $register_hotkey (i32.const 0x2000)))
    (call $set_panel_icon (i32.const 0x1A0) (i32.const 0x1A8))
    (call $log (i32.const 2) (i32.const 0x180))
    (i32.const 0))

  ;; ---------- process: zero the frame while muted ----------
  (func (export "process") (param $data i32) (param $samples i32) (param $ch i32) (param $qms f64) (result i32)
    (local $i i32)
    (if (i32.eqz (i32.load (i32.const 0x118))) (then (return (i32.const 0))))
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $samples)))
        (f32.store (i32.add (local.get $data) (i32.mul (local.get $i) (i32.const 4))) (f32.const 0.0))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i32.const 0))

  ;; ---------- handle_message ----------
  ;; 1) panel button:  {"action":"toggle"|"mute"|"unmute"}
  ;; 2) config:changed: {"key":"muted","value":true|false} 或 {"key":"shortcut","value":"..."}
  ;; 3) hotkey pressed: {"shortcut":"..."}
  (func (export "handle_message") (param $ptr i32) (param $len i32) (result i32) (local $tmp i32)
    (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x140) (i32.const 9))
      (then
        (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x168) (i32.const 8))
          (then (call $toggle_mute))
          (else
            (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x170) (i32.const 6))
              (then (call $set_mute (i32.const 1)))
              (else
                (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x176) (i32.const 8))
                  (then (call $set_mute (i32.const 0))))))))
        (return (i32.const 0))))
    (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x149) (i32.const 6))
      (then
        (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x160) (i32.const 7))
          (then
            (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x1B0) (i32.const 4))
              (then (call $apply_mute (i32.const 1)))
              (else
                (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x1B6) (i32.const 5))
                  (then (call $apply_mute (i32.const 0)))))))
          (else
            (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x108) (i32.const 8))
              (then
                ;; hotkey changed: re-read config & re-register so the new
                ;; shortcut takes effect immediately; the stale old hotkey
                ;; is filtered out in the hotkey branch below
                (local.set $tmp (call $get_config (i32.const 0x108)))
                (if (i32.gt_s (local.get $tmp) (i32.const 0))
                  (then
                    (call $copy_cstr (local.get $tmp) (i32.const 0x2100))
                    (if (call $valid_shortcut (i32.const 0x2100))
                      (then
                        (call $strcpy (i32.const 0x2100) (i32.const 0x2000))
                        (drop (call $register_hotkey (i32.const 0x2000)))
                        (call $log (i32.const 2) (i32.const 0x1C0))))))))))
        (return (i32.const 0))))
    (if (call $contains (local.get $ptr) (local.get $len) (i32.const 0x150) (i32.const 11))
      (then
        ;; hotkey pressed: compare the payload shortcut against the active one;
        ;; stale hotkeys left over from earlier registrations are ignored
        (call $extract_val (local.get $ptr) (local.get $len)
          (call $find (local.get $ptr) (local.get $len) (i32.const 0x150) (i32.const 11))
          (i32.const 11) (i32.const 0x2100))
        (if (call $eq_cstr (i32.const 0x2100) (i32.const 0x2000))
          (then (call $toggle_mute))
          (else (call $log (i32.const 1) (i32.const 0x1D8))))))
    (i32.const 0))

  (func (export "deinit"))
)
