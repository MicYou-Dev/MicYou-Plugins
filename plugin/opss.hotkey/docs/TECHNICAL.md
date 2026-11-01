# MicYou-Hotkey 技术文档 / Technical Deep-Dive

`opss.hotkey` — MicYou 原生插件：全局快捷键控制静音/耳返 + 真·按住说话/按住静音。
本文面向想读源码、改代码或审计行为的开发者。用户手册见 [README](../README.md)。

---

## 1. 问题与约束

MicYou 宿主为插件提供 Host API **v2** 控制面（C ABI v1，见
`micyou_plugin_abi.h`）：

* `set_muted` / `get_muted` / `set_monitoring` / `get_monitoring` —— 直接读写宿主
  音频控制面；
* 事件 `mute_changed {muted}` / `monitoring_changed {enabled}` —— 任何来源
  （UI、其他插件、本插件）改动都会广播，用于状态回读；
* `register_hotkey(shortcut)` —— 宿主全局快捷键。**基于 X11 grab**：
  * Wayland 会话下无效；
  * 只有**按下**事件，没有释放事件；
  * 不支持纯修饰键组合、不支持鼠标按键；
  * 宿主没有提供 unregister（注册过的 shortcut 常驻，见 §7）。

结论：`register_hotkey` 无法承载 Push-to-Talk / Push-to-Mute（按住 = 需要
down **和** up），在 Wayland 上连 toggle 都不可用。因此插件自带**平台原生
输入捕获层**（与 MicYou-Voice-Typing 相同路线），宿主 API 仅用作
*控制面* 与 *降级路径*。

### 线程契约（api-reference.md §调用约定）

宿主表指针只在 `init` 期间有效；插件的所有宿主调用必须发生在
**宿主调度的线程**上（init/deinit、`handle_message`、`handle_event`）。
自建线程（钩子线程、tap run-loop 线程、evdev 读取线程）**绝不触碰宿主表**，
只把中性事件写入 crossbeam channel，由宿主调度的 interval tick 泵出。

## 2. 总体架构

```
┌───────────────────────── 插件进程内（宿主进程的一部分） ─────────────────────────┐
│                                                                                  │
│  平台捕获线程（每平台一种，自建，不触宿主表）                                      │
│  ┌──────────────────┐   CaptureEvent::Edge{slot,edge}    ┌────────────────────┐  │
│  │ Windows: LL hook │──┐                                 │  crossbeam channel │  │
│  │ macOS:  CGEventTap│──┼──▶ 组合键匹配 + 边沿检测 ──────▶│   (unbounded)      │  │
│  │ Linux:  evdev poll│──┘   CaptureEvent::Status{...}     └─────────┬──────────┘  │
│  └──────────────────┘                                              │             │
│                                          宿主调度线程：interval:tick(20ms)         │
│                                                    pump() ◀────────┘             │
│  ┌───────────────────────────────────────────┐      │                            │
│  │ Core                                      │      ▼                            │
│  │  bindings[4]{raw,effective,error,active}  │  actions::Engine                  │
│  │  fallback / fallback_ids / fallback_active│   on_edge(slot,edge) → Command[]  │
│  │  status 镜像（节流写回 config.status）      │      │ exec()                     │
│  └───────────────────────────────────────────┘      ▼                            │
│                                    Host API v2: set_muted / set_monitoring /     │
│                                    notify / set_config / register_hotkey / log   │
└──────────────────────────────────────────────────────────────────────────────────┘
```

代码分层（`plugin/src/`）：

| 模块 | 职责 |
| --- | --- |
| `lib.rs` | FFI 入口（info/init/deinit/handle_event/handle_message）、`Core` 状态机、泵、降级仲裁、状态镜像、面板动作 |
| `abi.rs` | `mpl_host_api_t`/`mpl_plugin_info_t` repr(C) 定义 + 带空指针/错误检查的类型化包装（`with_host`） |
| `combo.rs` | 组合键文法：解析（`ctrl+alt+m`、`lctrl+lshift`、`mouse4`…）、校验规则、显示规范化、宿主字符串降级（`to_host_string`） |
| `actions.rs` | `Slot`（mute/monitor/ptt/ptm）、`Edge`（Press/Release）、`Engine`（纯函数式状态机，产生 `Command`）、PTT/PTM latch 语义 |
| `capture/mod.rs` | 平台无关层：`CaptureSettings` 快照（RwLock）、`EdgeDetector`（每 slot down/up 边沿去抖，吸收 auto-repeat）、`evaluate_all`、`report_status`、`start/shutdown` |
| `capture/windows.rs` | `WH_KEYBOARD_LL` + `WH_MOUSE_LL`，专用钩子线程 + 消息循环，超时自动 rehook，主键吞噬 |
| `capture/macos.rs` | `CGEventTap`（会话级、head-insert），专用 run-loop 线程，tap 被系统禁用后自动重启用，无辅助功能权限时降为 listen-only，主键吞噬 |
| `capture/linux.rs` | evdev（纯 Rust crate）枚举 `/dev/input/event*`，poll 多设备，启动时用 `get_key_state()` 同步初始按住状态，观察模式（不吞） |
| `capture/platform.rs` | 平台 ↔ 中性键位映射表（VK / CGKeyCode / linux input-event-codes） |

## 3. 组合键模型（combo.rs）

文法：`token ("+" token)*`，token 大小写不敏感。

* **修饰键**：`ctrl/alt/shift/win` → `AnyMod`（任一侧）；
  `lctrl/rctrl/lalt/ralt/lshift/rshift/lwin/rwin` → `ExactMod`（指定侧）。
* **主键** `MainKey`：`Letter(a-z)`、`Digit(0-9)`、`Func(1..=24)`、
  `Mouse4/Mouse5`、`Space/Enter/Tab/Esc/Backspace/Delete/Insert/Home/End/
  PageUp/PageDown/Up/Down/Left/Right/Minus/Equal/BracketLeft/BracketRight/
  Backslash/Semicolon/Quote/Comma/Period/Slash/Backquote/Pause`。

校验规则（解析期即拒绝，错误字符串进 `bindings[i].error` 供面板显示）：

1. 有主键且为字母/数字/普通键 → 至少一个修饰键；
2. 无主键（纯修饰组合）→ 至少两个修饰键，且不允许重复 token；
3. 无修饰键的裸主键 → 仅允许 `f1`–`f24`、`mouse4`、`mouse5`。

匹配语义：按下判定 = 所有修饰条件满足 **且** 其余修饰键均未按下
（`ctrl+alt+m` 不会被 `ctrl+alt+shift+m` 误触发）。修饰实时状态由各平台
自持（Windows `GetAsyncKeyState`、macOS `CGEventGetFlags`、Linux evdev 同步集）。

`to_host_string()`：把组合降级成宿主 `register_hotkey` 能接受的字符串
（`Ctrl+Alt+M` 风格）；纯修饰组合与鼠标键返回 `None`（宿主不支持）。

## 4. 动作状态机（actions.rs）

`Engine` 持有 `{muted, monitoring, ptt_latch, ptm_latch}`，纯函数式
`on_edge(slot, edge) -> Vec<Command>`（`SetMuted/SetMonitoring/Notify`），
便于单元测试且与宿主解耦：

* **toggle**：Press → 翻转并下发；
* **PTT**：Press 时仅当 `muted == true` 才 `SetMuted(false)` 并置
  `ptt_latch`（原本未静音 = passthrough，按键不影响状态）；Release 时仅当
  latch 在 → `SetMuted(true)` 恢复。auto-repeat 的重复 Press 被 latch 吸收；
* **PTM**：对称——Press 时仅当 `muted == false` 才 `SetMuted(true)` 置
  latch，Release 恢复；
* `shutdown_restore()`：deinit / 绑定热重载时，若仍持有 latch 则生成恢复
  命令——**绝不把麦克风留在热态（PTT 卡住）或死态（PTM 卡住）**。

宿主事件 `mute_changed` / `monitoring_changed` 到达时 `sync_*` 回写引擎
镜像（用户用 UI 或别的插件改了状态，我们的 toggle 基线要跟上）。latch 存在
期间的宿主翻转视为用户显式意图，latch 保持（松开仍恢复）。

## 5. 平台捕获后端

三个后端把平台事件翻译成同一组 `CaptureEvent`，共享 `EdgeDetector`：
每 slot 维护逻辑 down 状态，只有 false→true 报 `Press`、true→false 报
`Release`，组合中途按住的键变化会触发重评估（组合不再满足 = 逻辑释放，
PTT 提前恢复，防止“抬起修饰键后麦克风一直热着”）。

### Windows（`capture/windows.rs`）

* `SetWindowsHookExW(WH_KEYBOARD_LL)` + `WH_MOUSE_LL`，专用线程跑消息循环
  （LL 钩子回调在**安装线程**的消息循环上派发）；
* 钩子超时被系统静默摘除 → 循环里检测并 rehook；退出走 `PostThreadMessage(WM_QUIT)`；
* VK ↔ 中性键映射 + `GetAsyncKeyState` 读实时修饰状态；
* **吞噬**：热键命中时，主键的 down/up 返回 1（吃掉，不传前台）；修饰键不吞
  （吞修饰会破坏前台应用的组合键状态机）。鼠标侧键同理。

### macOS（`capture/macos.rs`）

* `CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap, …)`，事件掩码含
  keyDown/keyUp/flagsChanged/leftMouse*/rightMouse*/otherMouse* 与
  tapDisabledByTimeout/UserInput（被系统禁用后自动 `CGEventTapEnable` 重启用）；
* 专用线程 `CFRunLoopRun()`；shutdown 时 `CFRunLoopStop` + join；
* **辅助功能权限**（`AXIsProcessTrusted`）缺失时 tap 创建失败或以
  listen-only 建立：前者→先弹系统授权提示（`AXIsProcessTrustedWithOptions` +
  `kAXTrustedCheckOptionPrompt`）并报 `unavailable`（触发 §7 降级）；后者报
  `macos-tap-listen`（热键工作、不吞键）；
* ANSI 虚拟键码映射见 `keycode_of_main`（物理位置键码；F21–F24/Pause 无
  Apple 键码 → 该类组合在 macOS 上惰性并报 `error`，面板可见）；
* 修饰判定用 `CGEventGetFlags`：设备位（NX_DEVICE_*，区分左右）+ 设备无关
  掩码（NX_CONTROLMASK 0x40000 等）。**注意** `NX_DEVICERCTLbit = 0x2000`
  与 `NX_CONTROLMASK = 0x40000` 是两个不同的位（历史 bug 点，已加注释）。

### Linux（`capture/linux.rs`）

* 枚举 `/dev/input/event*`，按 `supported_events()`/`supported_keys()` 过滤出
  键盘与鼠标，`poll()` 多路复用 `fetch_events()`；
* 启动时用 `get_key_state()` 把**已经按住**的键同步进状态集（避免插件启动
  瞬间的组合误判/漏判）；
* evdev 是内核层原始 tap：**只观察，不注入、不吞噬**（吞键需要 uinput 重放
  剩余按键流，复杂度/风险不成比例；面板与 README 明示，推荐鼠标侧键/F13+/
  纯修饰组合规避）；
* X11 与 Wayland 通吃（不经过窗口系统）；权限：桌面会话 logind uaccess
  通常直接可读，否则 `input` 组；
* 无设备可读 → `unavailable` + 诊断（触发 §7 降级；Wayland 下宿主快捷键也
  不可用，此时真的只剩提示用户修权限）。

## 6. 泵与状态镜像（lib.rs）

* `set_interval(20ms, "hotkey-pump")`：宿主每 20ms 派发一次
  `interval:tick`（payload 过滤，防别的插件的 timer 串扰），tick 里
  `pump()`（drain channel → Engine → exec 宿主调用）+ `write_status(false)`；
* **状态镜像**：`config.status`（JSON object）持久化 `{engine, detail,
  enabled, muted, monitoring, pttActive, ptmActive, holdSupported,
  swallowSupported, fallbackActive, version, bindings{slot:{raw, effective,
  error, active}}}`。写入节流：变化后 ≥400ms 一次，最长 5s 强制一次；
  `write_status(true)` 为立即写（引擎状态变化、面板动作后）；
* 面板 `panel.html` 通过宿主 panel bridge（postMessage `__micyou:1`）调
  `get_config`/`set_config`/`trigger`；`trigger` 映射为
  `handle_message(topic="ui:<action>")`，支持 `test`（模拟一次完整
  press+release 走真实链路）、`apply`/`reload`、`status`、`log`；
* 热重载：宿主广播 `config:changed` → `pump()` + `reload()`——
  `shutdown_restore()` 先恢复 hold 基线，重读配置、重算 bindings、
  `update_settings()` 热替换捕获快照（后端线程无重启）、重新仲裁降级。

## 7. 降级路径（host-fallback）

`reconcile_fallback()` 在每次后端 `Status` 事件与 reload 后运行：

* 后端健康（engine ∉ {unavailable, starting, stopped}）→ 清空 fallback；
* 后端 `unavailable` → `fallback_active = true`，为 mute/monitor 两个
  **toggle** slot 调 `register_hotkey(to_host_string())`；PTT/PTM 无法降级
  （需要释放事件），面板 `holdSupported=false` 亮警告；
* **id 复用**：宿主没有 unregister，注册过的 shortcut→id 存
  `fallback_ids` 永久复用——重复注册同一 shortcut 会被宿主全局快捷键层
  当重复项拒绝。停用 = 我们不再理会该 id 的 `hotkey:<id>` 消息
  （`fallback` 列表清空即“ disarm”）；
* `handle_message` 收到 `hotkey:<id>`：查表 → 命中且 enabled → 以
  `Edge::Press` 走同一个 Engine（行为与原生路径一致，除了没有 Release）。

## 8. ABI 与生命周期

导出（C ABI v1，`#[unsafe(no_mangle)] pub extern "C"`）：

* `micyou_plugin_info()` → 静态 `mpl_plugin_info_t{abi_version: MPL_ABI_VERSION,
  api_version: 2, id: "opss.hotkey", version}`（宿主校验 ABI/API 版本与 id
  匹配后才 init；`process/handle_event/handle_message` 对宿主是可选符号，
  我们不导出 `process`——本插件不处理音频流）；
* `micyou_plugin_init(host)`：**按值拷贝**宿主表存入
  `HOST: Mutex<Option<mpl_host_api_t>>`（指针仅 init 期间有效）；重复 init
  先对旧实例 `shutdown()`；随后 load_config → `Engine::new(get_muted(),
  get_monitoring())` 取权威初值 → 设面板图标 → `capture::start` →
  `set_interval` 泵 → 首次状态写入；
* `micyou_plugin_deinit()` → `Core::shutdown()`：停捕获线程并 **join**
  （宿主可能随即 unload 库，绝不能留野线程）→ `shutdown_restore()` 恢复
  hold 基线 → `clear_interval` → 清 fallback → `clear_host()`；
* 所有入口过 `guard()`（`catch_unwind`）——插件跑在宿主进程内，panic 穿越
  C 边界是 UB，兜底降级为 `MPL_ERR_RUNTIME`；workspace release profile
  **禁用** `panic = "abort"`（同因）；
* `handle_event` 消费 `mute_changed` / `monitoring_changed` /
  `interval:tick` / `config:changed` / `hotkey:<id>` / `ui:<action>`。

### 重入分析

pump → `set_muted` → 宿主广播 `mute_changed` → 回到我们 `handle_event`：
宿主侧 dispatch 用 try_lock（重入时跳过），插件侧 `CORE` Mutex 同样
`try_lock` 风格串行化，且 handler 只做 `sync_*` + 状态镜像写——无环。

## 9. 测试策略

| 层 | 手段 | 覆盖 |
| --- | --- | --- |
| 单元（25 个，三平台 CI 全跑） | `cargo test`：combo 解析/校验/规范化、Engine 状态机（toggle/PTT/PTM/auto-repeat/shutdown_restore）、VK/CGKeyCode/evdev keycode 映射表、EdgeDetector | 平台无关逻辑 + 各平台映射常量 |
| E2E 控制面（开发期） | 开发期用本地宿主仿真器加载真实 `.so` 回放过脚本化时序（init → ui:test → config:changed 热重载 → 宿主 mute 翻转事件 → deinit），断言控制调用精确序列、interval 生命周期、状态镜像与降级注册，全部通过；仿真器不随仓库分发，CI 无对应自动化 | 生命周期、泵、热重载、事件同步、降级、状态镜像 |
| 真实输入注入 | **CI 无法自动化**（无图形会话、macOS 无授权）——Windows 钩子/macOS tap 的吞噬与 Linux evdev 的真机行为依赖代码评审 + 装机验收 | 已知缺口，如实声明 |

与真实 MicYou 宿主的集成验收：安装插件后逐一验证 ① 面板按键捕获录入 →
立即生效；② PTT/PTM 按住/松开恢复；③ 撤销 macOS 辅助功能权限 / Linux
`input` 权限观察降级路径；④ 宿主 UI 改静音后 toggle 基线跟随。

## 10. 打包与发布

* 单 zip 三平台：`micyou_hotkey.dll`(win x86_64) + `.so`(linux x86_64) +
  `.dylib`(macos aarch64)——宿主按扩展名解析 entry，同扩展名无法携带多
  架构，故 Linux arm / Intel Mac 不在产物矩阵（plugin.json `arches`
  如实声明 `["x86_64","aarch64"]`）；
* `scripts/package.sh`：本地开发包（Linux 原生 + mingw 交叉 Windows）；
* CI（`.github/workflows/release.yml`）：push → 三平台构建 + 单测 →
  development artifact；workflow_dispatch →
  可选 bump → tag + Release（资产 `plugin.zip` 固定名 + `plugin.json`）；
  `downloadUrl` 固定指向 `releases/latest/download/plugin.zip`，仓库
  manifest / zip 内 manifest / Release 资产三通道同一份。

## 11. 已知限制

1. Linux evdev 不吞键（观察模式）；
2. macOS 需要辅助功能权限才能吞键/完整捕获；未授权 → listen-only 或降级；
3. 降级模式（宿主快捷键）：X11-only、press-only → PTT/PTM 不可用，
   纯修饰/鼠标组合不可注册；
4. Windows UIPI：前台程序提权而 MicYou 未提权时收不到其按键；
5. macOS 无 F21–F24 / Pause 键码，此类组合惰性（面板报错提示）；
6. 架构矩阵见 §10；
7. 同扩展名单架构 = 一个安装只服务一个 (平台, 架构) 组合，由宿主解析规则决定。
