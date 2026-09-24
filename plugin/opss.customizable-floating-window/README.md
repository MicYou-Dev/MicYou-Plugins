# 🪟 可自定义悬浮窗 — Customizable Floating Window for MicYou

> A customizable floating window for MicYou status monitoring and control.

用 **Slint 1.18** 运行时编译用户自定义的 `.slint` 主题文件，绘制一枚透明、无边框、
置顶、不占任务栏的悬浮窗：实时展示 `audio.state` 数据与本次连接时长，
左键单击切静音、拖动移窗、右键交互——全部交互在 `.slint` 主题内定义。

<div align="center">
  <video src="https://github.com/user-attachments/assets/9eeae28e-4207-4b7c-b32f-7af50220d72a" 
         width="200" 
         autoplay 
         loop 
         muted 
         playsinline>
  </video>
</div>
*演示：悬浮窗实时状态与 WDIS 转录气泡（透明背景、置顶、不占任务栏）。*

## 安装

1. 下载 Release 的 `opss.customizable-floating-window-v<版本>.zip`
   （单包含三平台产物，宿主按平台自选；push 触发的 development artifact
   `…-v<版本>-development` 为未发布构建）；
2.  MicYou「设置 → 插件」安装该zip，启用「可自定义悬浮窗」。

目录结构：

```text
opss.customizable-floating-window/
├── plugin.json                     # 清单
├── customizable_floating_window.{dll,so,dylib}   # 插件核心（宿主按平台补后缀）
├── panel.html                      # 设置面板（主题切换/编辑器/状态）
├── bin/
│   ├── floating-helper-windows-x86_64.exe
│   ├── floating-helper-linux-x86_64
│   └── floating-helper-macos-aarch64             # Slint UI 子进程
└── themes/
    ├── ring.slint                  # 默认：复刻 MicYou v1 音量环
    ├── pill.slint                  # 信息条：电平条+时长+按钮
    └── minimal.slint               # 最小示例（自定义主题模板）
```

## 使用

* **左键单击**悬浮窗 = 切换宿主静音（未拖动阈值 4px）；
* **左键拖动** = 移动窗口（位置自动持久化，重启恢复）；
* **右键**：ring/minimal = 切换耳返；pill = 原生上下文菜单（静音/耳返/隐藏/重载）；
* **WDIS 语音转录**：启用后展示 WhatdidIsay 插件（本地离线识别）经其广播协议
  （魔数 `WDIS` 二进制帧、宿主消息总线）推送的识别文本——ring 为圆下方直径宽
  气泡、pill 为底部下伸面板，高度随文本自适应，定时收回；面板可调保持时长；
* **设置面板**（设置 → 插件 → 「悬浮窗设置」）：
  * 实时状态（串流/电平/静音/耳返/本次连接时长/设备/格式）；
  * 主题列表单选切换（热替换，不重启进程）、「热重载」、「恢复内置主题」、「位置复位」；
  * 在线 `.slint` 编辑器：保存即入列表；
  * 显示开关、刷新间隔（50–1000ms）。
* 直接增删 `themes/*.slint` 文件也可以：列表约 5 秒内自动刷新。

主题编译失败时：悬浮窗回退到内置 `ring.slint` 保证有窗可用，面板与系统通知给出诊断。

## 自定义主题（契约摘要）

主题 = 一个导出 Window 组件的 `.slint` 文件，所有契约成员可选（helper 内省后只绑定存在的）：

* 输入属性：`input-level` `processed-level` `smooth-level` `level-percent` `muted`
  `streaming` `monitoring` `sample-rate` `channels` `queued-ms` `session-seconds`
  `session-text` `device-label` `device-mode` `info-text` `arc-path` `bars-path`
* 输入属性（转录）：`wdis-text` `wdis-visible`
* 输出回调：`mute-toggle()` `monitoring-toggle()` `menu-action(string)`（仅 pill 菜单）
  `hide-window()` `drag-start()` `drag-move()` `drag-end()`（无参；helper 侧全局光标 1:1 跟踪）
* 窗口建议：`no-frame: true; always-on-top: true; background: transparent;`
  （“不占任务栏”由 helper 在窗口层实现，主题无需关心）

完整契约与示例见面板内「主题契约」折叠区与 [docs/TECHNICAL.md](docs/TECHNICAL.md)。

> ⚠️ Slint 1.18 的**软件渲染器不支持元素旋转**（`transform-rotation` 被静默忽略）。
> 斜线/旋转几何请用 `Path` 线段表达：静态 `commands`，或 helper 提供的
> `arc-path`（电平圆弧）/ `bars-path`（旋转波形条，~30Hz 相位）。

## 平台说明

| 平台 | 不占任务栏实现 | 备注 |
|---|---|---|
| Windows | `ITaskbarList::DeleteTab` + `WS_EX_TOOLWINDOW`/¬`WS_EX_APPWINDOW`（1Hz 重申） | winit 自身机制；详见 docs/TECHNICAL.md §13 |
| Linux X11 | `_NET_WM_STATE_SKIP_TASKBAR` + `SKIP_PAGER` | GNOME/KDE/XFCE 等 EWMH 合成器 |
| Linux Wayland | —（合成器策略，无客户端协议） | 多数 Wayland 桌面无任务栏概念；窗口拖动建议用 `WindowMoveArea` |
| macOS | `NSApplicationActivationPolicyAccessory` | 无 Dock 图标 / Cmd-Tab 条目 |

Linux 运行时依赖（桌面发行版默认均有）：`libx11-6`、`libxkbcommon0`、
`libxkbcommon-x11-0`、`libfontconfig1`。

## 构建与发布

CI（`.github/workflows/release.yml`，Focus-Capture 分发模型）：三平台矩阵构建
（Windows MSVC x64 / ubuntu x64 / macOS arm64），产物去 `lib` 前缀归一化后与三平台
helper 二进制、清单、面板、主题组装为安装包；**push** 产出 development artifact
`opss.customizable-floating-window-v<版本>-development`（zip 根即安装目录，不产生
release 资产）；**手动 workflow_dispatch** 才发 Release：bump 档位 → 防重复检查 →
回写版本化 `downloadUrl` → 版本化主包 `…-v<版本>.zip` + `plugin.json`（与 zip 内、
仓库根同一份 manifest）→ tag + Release + 变更日志。CI 内运行 actionlint 形式化检查。

本地验证：

```bash
cargo test                                   # 单元 + 主题编译契约测试
cargo build --release
target/release/floating_helper --compile-check themes/ring.slint   # 主题语法/契约报告
target/release/floating_helper --theme themes/ring.slint --selftest out  # 无宿主自测
tools/mock-host --plugin target/release/libcustomizable_floating_window.so \
                --dir <plugin-dir> --seconds 15    # 模拟宿主 E2E（含点击/拖拽注入）
```

实现细节、架构决策与验证记录见 [docs/TECHNICAL.md](docs/TECHNICAL.md)。

## 许可

GPL-3.0-only — 见 [LICENSE](LICENSE)。Slint GUI toolkit（© SixtyFPS GmbH）经其
GPLv3 路径使用；许可证选择缘由见 [docs/TECHNICAL.md](docs/TECHNICAL.md) §11。
