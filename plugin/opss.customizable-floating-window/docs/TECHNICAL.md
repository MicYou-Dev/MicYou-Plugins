# 技术文档 — `opss.customizable-floating-window` 实现详解

> A customizable floating window for MicYou status monitoring and control.
>
> 本文档记录：需求核对 → 可行性调研结论 → 架构决策 → 各模块实现 → 关键坑与解法
> → 资源利用设计 → 验证记录 → 打包发布。对应 MicYou 宿主 `master`
> （Host API v2，ABI v1）与 Slint **1.18.1**。

---

## 1. 需求与核对清单

| 需求 | 结论 | 实现位置 |
|---|---|---|
| Native 插件，Slint 1.18 启动时读取 `.slint` 绘制悬浮窗 | ✅ | helper 进程 `slint-interpreter 1.18` 运行时编译 |
| 用户可自定义 `.slint`；panel.html 切换可用 `.slint` | ✅ | `themes/` 目录 + 面板列表/编辑器 + 热替换 |
| 展示 `audio.state` 数据与本次连接时长 | ✅ | tick 拉取快照；`device_connected/disconnected` 事件 + streaming 回退 |
| 悬浮窗不占任务栏 | ✅（Wayland 除外，合成器策略） | helper 平台 FFI（§5.4） |
| 可互动：拖动 / 左键 / 右键菜单，**交互在 .slint 中定义** | ✅ | TouchArea/ContextMenuArea/WindowMoveArea + 回调契约（§6） |
| 可互动快速切静音的透明背景音量环（v1 样式） | ✅ | `themes/ring.slint`（v1 移植，§6.2）+ `control.intercept` |
| 三桌面平台 | ✅ | CI 三平台矩阵；helper 单进程模型解决 macOS 主线程约束（§3.1） |

## 2. 可行性调研（动手前的核对）

逐项在 Slint 1.18 源码/文档核实（仓库 `slint-ui/slint`，tag 区间 1.18.0/1.18.1）：

| 能力 | 结论 | 证据 |
|---|---|---|
| 透明无边框置顶窗 | ✅ | `Window` 属性 `background/no-frame/always-on-top`（官方 Window 参考） |
| 运行时编译用户 `.slint` | ✅ | `slint-interpreter::Compiler::build_from_source()`；`CompilationResult::components()`；`ComponentDefinition::{properties,callbacks,create}`；`ComponentInstance::{set_property,set_callback,invoke}` |
| 音量环圆弧（圆头线帽、动态扫过） | ✅（需 helper 供路径串） | `Path` 有 `commands/stroke/stroke-width/stroke-line-cap/viewbox-*`；Slint 表达式无 number→string，故弧路径由 Rust 侧生成写入字符串属性 |
| 左键单击切静音 / 点击≠拖动 | ✅ | `TouchArea.pointer-event/moved` + `pressed-x/y`、`mouse-x/y`；阈值逻辑写在 `.slint` |
| 拖动移窗 | ✅ 两条路 | 自定义：回调 → `slint::Window::set_position`；系统级：1.18 新增 `WindowMoveArea`（winit `drag_window()`） |
| 右键菜单 | ✅ | `ContextMenuArea + Menu/MenuItem`（原生菜单）与 `PopupWindow` 均可在解释器用（`item_registry` 注册了 ContextMenu/MenuItem，`popup.rs` 存在） |
| 不占任务栏 | ⚠️ Slint 无此属性 | 全源码无 `skip_taskbar`；`Window::window_handle()`（feature `raw-window-handle-06`）提供原生句柄 → 平台后处理（§5.4） |
| 解释器内交互元素 | ✅ | `internal/interpreter/item_registry.rs` 注册 TouchArea/WindowMoveArea/ContextMenu/MenuItem/Path/Timer 等 |
| 跨线程操作 UI | ✅ | `slint::Weak<T>: Send+Sync`（api.rs 1247/1250）+ `upgrade_in_event_loop` |
| macOS 主线程约束 | ⚠️ 架构级 | winit 事件循环必须主线程，而宿主 Tauri 占主线程 → **helper 子进程**（§3.1） |
| Slint 1.18 软件渲染器元素旋转 | ❌ 静默忽略 | `internal/renderers/software/lib.rs:3355 current_transform()` 仅平移 → 主题改用 Path 线段（§7.1） |

**结论：方案成立，无需中止。** 两个工程化前提（helper 子进程、任务栏平台 FFI）
均有干净解法，且交互逻辑仍 100% 由 `.slint` 定义。

## 3. 总体架构

```text
┌────────────── MicYou 宿主进程 (Tauri) ──────────────┐
│  Host API v2 (C ABI)                                │
│   audio_state / set_muted / set_monitoring /        │
│   get_config / set_config / set_interval / …        │
│        ▲                                            │
│        │ micyou_plugin_* (cdylib, 本仓库 plugin/)    │
│        │  · tick 状态机（interval:tick, 100ms）       │
│        │  · 会话计时 · 主题管理 · 重启退避             │
│        │  · 面板 ui:* 动作                            │
└────────┼───────────────────────┬────────────────────┘
         │                       │ stdin/stdout JSON lines (cfw-protocol)
         │                       ▼
│        │        ┌── helper 子进程 (本仓库 helper/) ──┐
│        │        │ main thread = Slint 事件循环        │
│        │        │  slint-interpreter 运行时编译主题   │
│        │        │  契约内省 → 属性/回调绑定            │
│        │        │  平台 FFI：任务栏/位置/屏幕/macos    │
│        │        └───────────────┬────────────────────┘
│        │                        ▼
│        │                 透明悬浮窗 (Slint 软件渲染)
```

### 3.1 为什么 helper 必须是子进程

1. **macOS 主线程铁律**：winit/Slint 事件循环只能跑在进程主线程；插件 cdylib 寄居
   的 Tauri 宿主主线程已被 tao 事件循环占用，进程内开第二 UI 线程在 macOS 直接不可行。
2. **宿主线程契约**：MicYou 文档明令 Host API 只允许在宿主分发线程调用，禁止插件自建
   线程调用。子进程 + 管道 IPC 把这条纪律变成**结构事实**而非自觉：helper 进程里根本
   没有 Host API 符号。
3. **崩溃隔离**：用户 `.slint` 运行时编译 + GPU/驱动渲染栈有任何故障，死的是 helper；
   插件核心按退避重启它，宿主无感（§4.4）。
4. **卸载安全**：cdylib `deinit` 后库被 unload，若 UI 线程还在跑本库代码即崩溃；
   子进程天然无此问题（deinit 只需 kill _child_）。

代价：约 30–60 MB 常驻 RSS（Slint 软件渲染进程）与一次管道跳转（≤ tick 周期）。
`visible=false` 时 helper 完全退出释放内存（隐藏=挂起语义）。

### 3.2 IPC 协议（`protocol/` crate，JSON lines）

plugin → helper（stdin）：`State{…}` / `Theme{path}` / `Visible{show}` /
`Pos{x,y}` / `PosDefault` / `Snapshot{path}` / `Quit`
helper → plugin（stdout）：`Ready{theme,reloaded}` / `ThemeError{message}` /
`Mute` / `Monitoring` / `Menu{action}` / `Hide` / `Moved{x,y}` / `Log{message}` / `Bye`

* serde `tag="cmd"/"ev"` + 忽略未知字段 → 新旧版本互容（前向兼容）。
* stdout 只走协议；诊断一律 stderr（插件侧保留尾 12 行用于崩溃事后日志）。
* 孤儿互防：宿主死 → helper stdin EOF 自退；helper 死 → 读线程 EOF → 插件退避重启。

## 4. 插件核心（`plugin/`，cdylib）

### 4.1 ABI 与版本

* `mpl_plugin_info_t`：`abi_version=1`、`api_version=2`（使用 v2 控制面字段
  `set_muted/get_muted/set_monitoring/get_monitoring`；宿主
  `MIN_SUPPORTED=1..=HOST=2`，v1 老宿主会以错误码 7 干净拒载，而不是越界读表）。
* host 表按值拷贝进 `HOST: Mutex<Option<mpl_host_api_t>>`（init 指针仅调用期有效）。
* v2 追加字段仍声明为 `Option<fn>` 并判空：个别槽位为空的宿主优雅降级。
* **不导出 `micyou_plugin_process`**：本插件 `kind=ui`，永不进实时音频线程。

### 4.2 能力（最小化）

`audio.state`（状态快照）、`control.observe`（get_muted/get_monitoring +
mute_changed 等事件）、`control.intercept`（set_muted/set_monitoring）、
`config.read/config.write`（主题选择/位置/状态镜像）。
不申请 fs.*：主题读写直接用 `plugin_dir()` + 标准 fs 且严格限定自身目录
（文件名白名单校验 `valid_theme_name`：长度/字符集/后缀/防穿越）。

### 4.3 tick 状态机（`interval:tick`，默认 100ms）

单一 interval（payload 标签 `cfw` + interval id 双重过滤，防宿主残留旧定时器串扰）。
每 tick 顺序：

1. `supervise_helper()`：死亡检测（`try_wait` 非阻塞）→ 退避重启（0.5s·2ⁿ，封顶 15s，
   连续 6 次放弃并通知）；健康 ≥60s 清零计数；
2. `handle_helper_events()`：drain 事件队列 →
   `Mute/Monitoring` → 控制面调用（用最近缓存态取反）；`Menu{reload-theme}`；
   `Hide` → 持久化 `visible=false`；`Moved` → 节流写 `windowX/Y`；
   `Ready` → 清 `themeError`、复位退避；`ThemeError` → 写配置 + 去重通知；
3. `audio_state()` → 会话 `observe_streaming`（回退推断）→ 组装 `StatePayload`
   → 推 helper stdin（写线程持有管道，tick 永不阻塞）→ 状态镜像（§4.5）；
4. 每 10 tick：配置轮询（theme/visible/updateMs/windowX/Y 漂移 → 热替换/启停/改周期）；
5. 每 50 tick：`themes/` 扫描 → `themeList` 变化才写配置。

所有 Host API 调用都发生在宿主分发线程（init/deinit/handle_event/handle_message）。
三条自有线程（stdin 写 / stdout 读 / stderr 尾）**只碰管道与队列**。

### 4.4 会话计时（“本次连接时长”）

* 权威源：`device_connected{mode,label}` / `device_disconnected` 事件
  （宿主 TCP 控制通道广播）；
* 回退源：插件在连接建立**之后**才启用时收不到事件 → 用 `audio_state.streaming`
  的 false→true 跳变开场；仅对“推断会话”生效的 30 tick 无串流宽限结束它
  （传输抖动不复零）；权威事件随时接管/升级 label。
* 展示：`session-seconds` + 预格式化 `session-text`（Slint 无 number→string）。

### 4.5 配置键与面板桥

| 键 | 写方 | 用途 |
|---|---|---|
| `theme` `visible` `updateMs` | 面板/宿主表单 | 用户意图（tick 轮询 + `ui:apply-theme` 即时） |
| `windowX/Y`（null=自动右上） | 插件（Moved 节流 1s + deinit 落盘） | 位置持久化 |
| `themeList` `themeError` `helperState` `status` | 插件（变化门控，≥1s） | 面板展示镜像 |
| `themeDraft` `themeDraftName` | 面板 | 编辑器保存/载入中转 |

面板桥动作（topic `ui:<action>`）：`apply-theme` `reload` `restart-helper`
`show` `hide` `reset-pos` `save-theme` `stage-theme` `restore-themes` `snapshot` `log`。

### 4.6 panic 与错误边界

所有 FFI 入口 `catch_unwind(AssertUnwindSafe)` → `MPL_ERR_RUNTIME`；
**release profile 永不 `panic=abort`**（cdylib 在宿主进程内，abort 会带走 MicYou）。

## 5. helper 子进程（`helper/`）

### 5.1 运行时主题编译与契约内省

`Compiler::build_from_source()`（pollster 驱动）→ 首个导出组件 →
`properties()/callbacks()` 收集成员名集合 → **只绑定存在的成员**
（`apply_state/apply_smooth/attach_callbacks` 逐一查表），因此任意子集主题皆可运行。
编译失败：初次加载回退内置 `ring.slint`（`include_str!` 保证总有窗）并报
`ThemeError`；热替换失败则**保留当前窗**只报错。

### 5.2 数据流与平滑

* `State` 命令 → 全量属性即时写入（bool 必须跟手）；
* 33ms UI Timer：EMA 平滑（k=0.3，ε 吸附）→ 仅当可见变化时写
  `smooth-level/level-percent/arc-path/bars-path`（30Hz，字符串重建开销 µs 级）；
  串流且未静音时推进波形相位 +6°/tick（v1 的旋转条动画移到 helper 侧，
  原因见 §7.1）；
* 1s UI Timer：位置轮询（`WindowMoveArea` 系统拖动/WM 移动的唯一感知途径；
  自定义拖动期间抑制；Wayland  dummy (0,0) 过滤）。

### 5.3 自定义拖动协议（点击≠拖动）

`.slint` 侧：`down → drag-start()`；`moved → drag-move(Δ逻辑px)`；
`up` 且 `max-move<4px → mute-toggle()`，否则 `drag-end()`。
helper 侧：`drag-start` 记录 `window().position()`+`scale_factor`；
`drag-move` → `set_position(base + Δ·scale)`（物理像素）；`drag-end` →
`Ev::Moved` 持久化。Wayland（`set_position` 不可用）自动降级为 no-op，
主题可改用 `WindowMoveArea`（pill 即如此）。

### 5.4 “不占任务栏”平台后处理（Slint 无此能力）

show 后 150ms 起单shot 重试链（≤12 次，等窗口映射出句柄）：

* **Windows**：`ITaskbarList::DeleteTab(hwnd)` + `WS_EX_TOOLWINDOW`/¬`WS_EX_APPWINDOW`
  （show 后首应用 + 1Hz 重申；winit 自身机制，详见 §13——早期的纯 exstyle 做法
  被证明结构性无效）；
* **X11**：XlibWindowHandle→`XGetWindowProperty(_NET_WM_STATE)` **追加**
  `_NET_WM_STATE_SKIP_TASKBAR`+`_NET_WM_STATE_SKIP_PAGER`（不覆写 WM 已置位，
  如 ABOVE）→`XChangeProperty`+`XFlush`（x11-dl dlopen，无链接期依赖）；
* **macOS**：`NSApplication setActivationPolicy:(Accessory)`（raw objc msgSend；
  启动时 + show 后各断言一次——winit 初始化 NSApp 时会重置为 Regular）；
  效果：无 Dock 图标/无 Cmd-Tab，窗口仍可交互；
* **Wayland**：无客户端协议，no-op 并记日志（合成器策略；多数 Wayland 桌面无任务栏）。

### 5.5 默认位置与多屏

无保存位置时取主屏物理尺寸（Win `GetSystemMetrics` / macOS
`CGDisplayPixelsWide/High` / X11 `XDisplayWidth/Height`）− 窗口尺寸 − 24px 边距
→ 右上角（v1 同款）。保存位置为全局物理坐标，支持负坐标副屏
（`x<0 且 y<0` 保留为“自动”哨兵；面板「位置复位」写 null）。

### 5.6 快照与自测

`Snapshot{path}` → `Window::take_snapshot()` → png crate 写 RGBA PNG
（含 alpha）。`--selftest <prefix>` 无宿主合成三态快照；`--compile-check`
输出契约报告（CI 与主题作者工具）。

## 6. 主题与 v1 移植

### 6.1 契约（全部可选；面板内含完整表）

属性：`input-level processed-level smooth-level level-percent muted streaming
monitoring sample-rate channels queued-ms session-seconds session-text
device-label device-mode info-text arc-path bars-path wdis-text wdis-visible`
回调：`mute-toggle() monitoring-toggle() menu-action(string) hide-window()
drag-start() drag-move() drag-end()`（drag 三回调无参：helper 侧全局光标增量跟踪，
密集鼠标事件下仍 1:1；见 §12.3）

### 6.2 `ring.slint` ↔ v1 `FloatingMicWindow` 对照

| v1 (Compose Canvas) | ring.slint |
|---|---|
| 36dp 透明无边框置顶圆窗（undecorated/transparent/alwaysOnTop） | 84px `Window{no-frame;always-on-top;background:transparent}` |
| 底环 `drawCircle(alpha .15/.3)` | `Path` 双半弧整圆，alpha 随状态 |
| 电平弧 `drawArc(-90°, 360°·level, cap=Round)` | `Path{commands: arc-path; stroke-line-cap: round}`（helper 归一化 100×100 视图框，r=45） |
| 8 根旋转波形条（2s 相位） | `Path{commands: bars-path}`（helper 30Hz 相位，公式同 v1 `0.3+0.7·sin(…)`） |
| 中心辉光 ∝ level | 圆 `d=28px·lvl`，alpha ∝ lvl |
| 静音斜杠 | 静态对角 `Path "M 33 33 L 67 67"`（软件渲染器无旋转，§7.1） |
| 空闲淡环+中心小圆 | 同 |
| MouseAdapter：5px 阈值拖动/点击切静音 | TouchArea 4px 阈值 + drag 三回调（§5.3） |
| —（v1 无菜单） | 右键 `ContextMenuArea`（静音/耳返/隐藏/重载） |

### 6.3 `pill.slint` / `minimal.slint`

pill：224×56 半透明信息条——状态点（串流呼吸）、电平条（`animate width`）、
`session-text`+设备/格式文本、MIC/MON 按钮、`WindowMoveArea` 系统拖动、右键原生菜单。
minimal：30px 圆点最小契约示例（自定义主题起点模板）。

## 7. 关键技术发现与决策记录

### 7.1 Slint 1.18 软件渲染器忽略元素变换 ⚠️

实测（dbg 快照像素级验证）：`transform-rotation/transform-scale` 在
`renderer-software` 下**静默无效**（`current_transform()` 仅平移）。
FemtoVG/Skia 支持变换，但引入 GL/Vulkan 依赖会牺牲“任何机器都能跑”
（无 GPU 的 VM/CI/老驱动）。**决策**：坚持软件渲染器；旋转/斜线几何一律
用 `Path` 线段表达——静态 `commands`（斜杠）或 helper 生成的
`arc-path/bars-path`（动态）。该限制写入面板契约与 README。

### 7.2 `HOST` Mutex 非可重入死锁（mock-host 抓出）

首版 `abi::read_host_string` 在 `with_host` 闭包内再次 `with_host` →
std Mutex 自锁，init 永久挂起。**mock-host E2E 第一轮即暴露**。
修复：先 `with_host(|h| *h)` 按值拷贝表（`mpl_host_api_t: Copy`）释放锁，
再调回调。该 bug 印证了“用真实 ABI 镜像做 E2E”的价值。

### 7.3 macOS 激活策略被 winit 覆盖

winit 建事件循环时把 NSApp 策略重置为 Regular → 单靠启动时设置无效；
show 后重试链内**再断言一次** Accessory（§5.4）。

### 7.4 残留定时器串扰

宿主 disable 插件后其 interval 线程可能仍投 tick（按插件 id 路由）。
双重过滤：payload 标签 `cfw` + `interval id == self.interval_id`，陈旧 tick 直接丢弃。

### 7.5 Xvfb 无 WM 的系统拖动

`WindowMoveArea` 依赖 EWMH `_NET_WM_MOVERESIZE`，裸 Xvfb 无 WM → no-op。
E2E 交互断言因此放在 ring（自定义拖动，不依赖 WM）；pill 的 WindowMoveArea
在真实桌面正常。文档注明。

## 8. 资源利用设计

| 项 | 设计 |
|---|---|
| tick 频率 | 默认 100ms（面板可调 50–1000）；`audio_state` 为原子读+try_lock，µs 级 |
| 属性写 | 30Hz 但带 ε 门控（无可见变化不写）；bool/文本随 State 即时 |
| 配置写盘 | 状态镜像 ≥1s 且内容变化；位置 ≥1s 节流 + deinit 落盘；themeList 仅变化时 |
| 管道 | stdin 独立写线程（tick 不阻塞）；事件队列上限 64 溢出丢旧 |
| 内存 | helper 隐藏即退出；插件核心无堆增长点（队列/尾日志有界） |
| 崩溃 | 退避 0.5s→15s，6 次放弃+通知；健康 60s 复位 |
| 二进制 | helper：opt-level 2 + lto thin + strip；cdylib 不 abort（§4.6） |

## 9. 验证记录（本仓库可复跑）

1. `cargo test`：18 项通过——协议 round-trip/前向兼容、会话计时四态、
   弧几何（0/¼/½/¾/满/钳制/单调）、bars 八段、**三内置主题编译+实例化+契约断言**、
   坏主题回退、主题名白名单、helper 路径解析。
2. `--compile-check`：三主题契约报告（minimal 只实现子集 → 验证“全可选”设计）。
3. Xvfb `--selftest`：三主题×三态 PNG 快照像素级目检（`dist/shots/themes-sheet.png`）；
   `window flags applied: skip-taskbar-x11` 日志确认 X11 任务栏位写入。
4. **mock-host E2E**（真实 cdylib + 真实 helper + X11）：
   init→theme ready→device_connected→会话计时→`ui:apply-theme` 热替换 ring→pill→
   快照；**xdotool 真实点击**两次 → `*** CONTROL-PLANE set_muted(true/false) ***`；
   **真实拖动** → `windowX=858/windowY=358`（与几何推算一致）→ deinit 干净退出。
5. 交叉目标：`cargo check --target x86_64-pc-windows-gnu` 与
   `--target aarch64-apple-darwin`（CI 再做真构建）。

## 10. 打包与 CI

* 单 zip 跨平台（Focus-Capture 模式）：三 cdylib 同名去 `lib` 前缀（宿主按平台补
  后缀）+ `bin/floating-helper-{windows-x86_64.exe, linux-x86_64, macos-aarch64}`
  + 清单/面板/主题；
* CI 矩阵：windows-latest(MSVC x64) / ubuntu-latest(x64, +libfontconfig1-dev 等) /
  macos-latest(arm64)；步骤：actionlint → test → build → 主题 compile-check →
  归一化 → 组装；
* **push** → development artifact `opss.customizable-floating-window-v<ver>-development`
  （zip 根即安装目录；不产生 release 资产）；**workflow_dispatch** → release 任务
  （bump 档位/防重复/回写版本化 downloadUrl/版本化 zip + plugin.json/tag/变更日志）；
* `plugin.json` 为安装/发布/市场同一份 manifest：`repository`/`homepage`/`updateUrl`
  （→ `…/releases/latest/download/plugin.json`）/`readmeUrl` 已指向
  `OrientCOMPASS/Customizable-Floating-Window`。

> 内存说明：release profile（`codegen-units=1` + LTO）在 ≤1 GB 内存的容器里
> 编译 `x11rb-protocol` 会被 OOM-kill（与本插件代码无关）。本地小内存环境用
> `--profile sandbox`（同源码、更保守的代码生成）或 mingw 交叉构建；
> 正式 release 产物由 CI 的大内存 runner 产出。

## 11. 源码地图

```text
protocol/src/lib.rs        Cmd/Ev + 行编解码（双端共用，防漂移）
plugin/src/abi.rs          v2 host 表镜像 + 缓冲契约包装（值拷贝/非可重入修复）
plugin/src/session.rs      连接计时（事件权威 + streaming 回退 + 宽限）
plugin/src/helper.rs       子进程生命周期（三管道线程/退避/kill 宽限）
plugin/src/lib.rs          FFI 入口 + tick 状态机 + 面板动作 + 主题目录管理
helper/src/main.rs         事件循环编排/热替换/拖动/轮询/快照/selftest
helper/src/theme.rs        运行时编译 + 契约内省绑定 + 回退
helper/src/arc.rs          弧/波形条 SVG path 生成（含单元测试）
helper/src/platform.rs     任务栏 FFI/屏幕尺寸/macOS 策略/Wayland 探测
helper/src/ipc.rs          stdout 协议出口 + panic hook
themes/*.slint             三内置主题（契约示例 + v1 移植）
panel.html                 设置面板（桥 API：get_config/set_config/trigger/locale）
tools/mock-host/           宿主 ABI 镜像 E2E 台（含 interval 泵/事件时间线）
```


---

## 12. Round-7：基线重建（9598f65）+ 三项增量

用户指定 `9598f657` （“最接近目标”的稳定版：helper 子进程 + exstyle 配方，唯一瑕疵为
任务栏条目）为新基线；在其上仅做三项增量，不再改动已稳定的架构与窗口配方。

### 12.1 WDIS 语音转录接入（WhatdidIsay 广播协议）

* 协议：`b"WDIS" | start_ms i64 LE | end_ms i64 LE | UTF-8 text`，经宿主消息总线广播；
  消费方仅校验 magic（topic 不固定）→ 插件在 `handle_message` 入口**先于 topic 路由**
  做 magic 检查；
* 插件 → helper：`Cmd::Wdis{text, hold_ms}`（stdin）；helper 在 UI 线程写入可选契约
  成员 `wdis-text`(string) / `wdis-visible`(bool)，并起 `hold_ms` 单次定时器收回；
* 主题形态：**ring** = 圆下方伸出「宽 = 直径」文本框（`box-h` 260ms 伸出/收回 +
  `box-alpha` 420ms 渐变消失，窗口高度 `win + box-h` 向下生长、顶边锚定）；
  **pill** = 底部下伸面板（`box-h` 240ms）；
* 配置：`wdisEnabled`（默认开）/ `wdisHoldMs`（默认 4000，1–30s），面板「选项」卡
  含开关 + 保持时长滑杆 + 含义说明（不裸写 WDIS 缩写：说明其来自 WhatdidIsay 插件、
  魔数 WDIS 二进制帧、宿主总线广播、两主题展示形态）；
* 验证：mock-host `MOCK_WDIS` 注入真实二进制帧 → 插件日志收帧 → helper 落属性 →
  selftest 快照 84×124（窗口确实长高）+ E2E 全链路标记齐全。

### 12.2 release.yml：Focus-Capture 分发模型（彻底版）

* push → `build` + `dev-package`（**平级任务**）：artifact 名
  `opss.customizable-floating-window-v<ver>-development`（带版本号；zip 根即安装目录）；
  development **不产生任何 release 资产**；
* workflow_dispatch → `release`（与 dev-package 平级，同 `needs: build`）：bump 档位
  （none/patch/minor/major）→ `gh release view` 防重复 → 打包前回写版本化
  `downloadUrl` → 版本化主包 `opss.customizable-floating-window-v<ver>.zip` →
  tag + `gh release create`（zip + plugin.json 同一份 manifest）+ 自上一 tag 变更日志；
* 矩阵三平台（无 macOS x86_64）；三平台均携带 helper 二进制（bin/）；
* CI 内 actionlint 形式化检查；本地 actionlint 零告警。

### 12.3 .slint 调整修复

* **拖动 1:1**：基线的 v2 协议（主题上报窗内相对坐标、helper 用 `P0 + Δ` 落位）存在
  反馈环：窗口移动后相对坐标自抵消，数学上 `e_{n+1} = −e_n` 振荡——稀疏事件（xdotool
  大步 warp）下恰好收敛掩盖问题，真实鼠标的密集事件表现为“移动一半 + 跳动”。
  修复为**事件驱动全局光标增量**：`drag-move`（无参）触发时查询平台全局光标
  （Win `GetCursorPos` / X11 `XQueryPointer` / macOS `NSEvent mouseLocation`），
  按 `P += C − C_last` 移窗——光标坐标系与窗口位置无关，反馈环结构性消失。
  验证：**10 步 ×30px 密集步进拖动** → `windowX/Y = 872/324`，与光标增量逐像素一致；
* **右键 = 切换耳返**（ring/minimal）：移除两者的 `ContextMenuArea` 菜单，
  `pointer-event` 右键抬起 → `monitoring-toggle()`（E2E `set_monitoring(true)` 标记）；
  pill 保留 `WindowMoveArea` 系统拖动 + 原生右键菜单；
* **内置主题升级策略**：首行 `// cfw-bundled: <rev>` 标记；init 时“缺失→写入、
  带标记且不同→升级覆盖、无标记→用户财产不动”；
* 点击≠拖动判定改为**移动事件计数**（<2 次 = 单击）：全局光标跟手下窗内相对坐标
  几乎不变，旧的位移阈值失效。

### 12.4 验证汇总（round-7）

18 项单元/契约测试全过；三主题 compile-check OK；Xvfb E2E：密集拖动 1:1、单击静音、
右键耳返、WDIS 注入、`_NET_WM_STATE_SKIP_TASKBAR` 在位、selftest 快照 84×124、
deinit 干净；`x86_64-pc-windows-gnu` 交叉构建通过；actionlint 零告警；
`dist/plugin-linux+windows.zip`（macOS 产物由 CI 出）。

### 12.5 Round-8：WDIS 文本区视觉重做 + panel 排版修复（用户实测反馈）

* **高度自适应**：两主题文本区高度改由 `Text.preferred-height` 驱动
  （`box-h = wdis-visible ? (wtxt.preferred-height + pad) : 0px`，`animate` 伸出/收回），
  文本长短决定窗口生长量——取代固定 40/34px；
* **ring 风格匹配**：深色底浅字 → **白底深字 + 状态色描边圆角气泡**（`#ffffffeb` +
  `content.with-alpha(0.35)` 描边），与圆环的明亮轻盈一致（浅色桌面合成预览见
  `dist/shots/wdis-light-preview.png`）；
* **pill 结构修复**：主栏 `HorizontalLayout` 固定 `height: 56px`（否则窗口生长时布局
  铺满全窗与文本重叠——用户截图的“灾难”根因）；转录区改为卡片内**分隔线 + 内边距**
  的统一圆角卡片结构（`dist/shots/pill-wdis-light.png`）；
* **panel.html 排版修复**：`.row` 增加 `flex-wrap: wrap`；hint 文本独立成块
  （`flex-basis: 100%`，不再被挤进按钮行成竖排）；按钮组改 `.actions` 换行容器；
  `code/pre` 增加 `overflow-wrap: anywhere`；「优先使用 AI/LLM 修改主题」说明块
  （含主题目录路径）置于编辑器卡片内、主题契约折叠区上一行；中英本地化键补齐
  （aiTipTitle/aiTipBody/pathLabel/editHint/lblWdis/lblWdisHold/wdisExplain）。

---

## 13. Round-9：Windows 任务栏问题根因与最终修复（逐条说明）

### 13.1 证据链（社区讨论 + winit 源码）

1. **slint-ui/slint discussion #3266**（"How to hide the window in the taskbar?"）：
   提问者与我们的旧做法完全相同——show 之后 `SetWindowLongPtr(GWL_EXSTYLE, WS_EX_TOOLWINDOW)`，
   结果**任务栏按钮不消失**（"I failed to hide my window in the taskbar… or the window
   flag is changed somewhere else"）；维护者 tronical/ogoffart 确认：Slint 无开箱 API，
   只能经 `i-slint-backend-winit` 私有路径碰 winit。
2. **winit 0.30 源码** `platform_impl/windows/window.rs`：winit 自己的
   `set_skip_taskbar()` **不改扩展样式**，而是 COM 接口
   `ITaskbarList::DeleteTab(hwnd)` / `AddTab(hwnd)`。
3. **winit `window_state.rs`**：`WindowFlags::apply_diff()` 在**任何** flag diff 时
   （show 时的 VISIBLE、置顶变化等）用 winit 内部 flag **整体重算并重写**
   `GWL_STYLE`/`GWL_EXSTYLE`，且 `ON_TASKBAR` 默认置位 → show 时加
   `WS_EX_APPWINDOW`。⇒ 外部预设的 TOOLWINDOW 位会在 show 那一刻被覆写；
   show 之后改 exstyle 则按钮已创建、改位不删除按钮。两种旧做法**结构性必败**。
4. 任务栏按钮由 shell 在窗口**变可见时**创建 ⇒ 正确时机只有二：show 前（被 3 覆写，
   不可用）或 show 后用 DeleteTab 删除（winit 认可机制，且不受样式重写影响）。

### 13.2 修改清单（每个细节）

* **`helper/src/platform.rs` Windows 段重写**（唯一功能性改动面）：
  1. 新增 raw COM 声明（不引 windows crate）：`Guid`（{56FDF344-FD6D-11D0-958A-006097C9A090}
     = CLSID_TaskbarList；{56FDF342-…} = IID_ITaskbarList）、`ITaskbarListVtbl`
     （COM ABI 顺序：IUnknown 的 QueryInterface/AddRef/Release + ITaskbarList 的
     HrInit/AddTab/DeleteTab/MarkFullscreenWindow/SetActiveAlt）、`CoInitializeEx` /
     `CoCreateInstance`（ole32）、`GetWindowLongPtrW/SetWindowLongPtrW/GetSystemMetrics`（user32）。
  2. `TASKBAR_LIST: AtomicIsize` 进程级缓存：接口指针按公寓（apartment）归属，
     我们永远在同一 UI 线程调用 ⇒ 缓存安全；不 Release（进程寿命持有）。
  3. `taskbar_delete_tab(hwnd)`：`CoInitializeEx(NULL, COINIT_APARTMENTTHREADED)`
     （引用计数式，已初始化返回 S_FALSE 无视）→ `CoCreateInstance(CLSID_TaskbarList,
     CLSCTX_ALL, IID_ITaskbarList)` → `HrInit` 一次 → `DeleteTab(hwnd)`；失败返回 false。
  4. `windows_skip_taskbar(window)`（幂等）：`raw-window-handle` 取 HWND 并缓存
     （`OVERLAY_HWND`）→ `DeleteTab` + `exstyle |= WS_EX_TOOLWINDOW &= !WS_EX_APPWINDOW`
     （后者防 shell 在 explorer 重启等场景重建按钮，并顺带退出 Alt-Tab）；
     返回描述串供日志。
  5. **删除**旧无效操作：caption/style 剥离（winit 创建期已按 `no-frame` 生成无边框，
     事后剥离会被 apply_diff 覆写）、`SetWindowPos(SWP_FRAMECHANGED)`（无必要）、
     TOPMOST 周期刷新（Slint `always-on-top` → winit `ALWAYS_ON_TOP` 已管理）。
     全程**无** `ShowWindow(SW_HIDE/SW_SHOW)`（round-3/4 灰帧残影根因）。
* **`helper/src/main.rs` 3 行**：1Hz `pos_timer` 回调开头 `#[cfg(windows)]`
   重申 `apply_skip_taskbar`（DeleteTab 幂等、µs 级；治愈 explorer 重启后的按钮复活）。
* **调用链不变**：show 后 ~150ms 重试链首应用（12 次重试等 handle 可用）；
   热替换产生新窗口时重试链再次应用（E2E 日志 0.22s 与 4.26s 两条即此）。
* **X11 / macOS / Wayland 路径零改动**（X11 的 `_NET_WM_STATE_SKIP_TASKBAR` 为 WM
   管理、持久有效；macOS Accessory 为进程级；Wayland 无客户端协议）。

### 13.3 行为与边界

* Alt-Tab：TOOLWINDOW 位存活期间一并排除；若未来 winit apply_diff 覆写该位，
  任务栏仍由 1Hz DeleteTab 兜底（Alt-Tab 可能回归——可接受的取舍）。
* COM 失败（极罕见）降级为仅 exstyle，日志明示。
* 验证：`x86_64-pc-windows-gnu` 交叉 check 零告警；Linux E2E 交互/标志/热替换全绿；
  18 项测试全过；zip 重打包（含新 Windows helper）。
