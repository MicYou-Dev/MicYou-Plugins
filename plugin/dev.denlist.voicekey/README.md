# VoiceKey — 语音自动按键 · MicYou 原生插件

[![runtime](https://img.shields.io/badge/runtime-native-blue)](https://github.com/MicYou-Dev/MicYou)
[![platform](https://img.shields.io/badge/platform-Windows%20x86__64-lightgrey)](#)
[![license](https://img.shields.io/badge/license-Unlicense-green)](LICENSE)

对着手机说话，插件自动按住 `Ctrl+Win`（微信输入法 / 搜狗等的「按住说话」语音输入快捷键），说完自动松开。让你在用 MicYou 把手机当麦克风时，**免提完成语音输入**，不用手去碰键盘。

## 解决什么问题

MicYou 把手机变成电脑的麦克风后，想用微信输入法的语音输入，通常要手动「按住」语音键——但你正在看屏幕/说话，腾不出手。VoiceKey 检测到有语音（电平超过阈值）就替你按住，静音后替你松开，完全模拟「按住说话」。

## 功能

- 🎙️ **真实按住说话**：检测到语音 → 按住 → 静音 → 松开，与微信输入法语义一致。
- ⌨️ **按键可自定义**：面板里直接录组合键（Ctrl/Win/Alt/Shift/F 键/字母/数字…），默认 `Ctrl+Win`。
- 📊 **实时电平条 + 状态灯**：面板实时显示当前电平、是否在按住。
- 🛡️ **保险机制**：最短/最长按住时长、静音超时、设备断开自动松开——绝不卡键。
- 🔇 **不改动音频**：`kind=utility`，只读取音频电平做触发，不进入 DSP 链，不影响音质。

## 安装

### 方式一：插件市场
在 MicYou 的插件市场里搜索 **VoiceKey** 安装。

### 方式二：手动导入
1. 下载本仓库 Release 里的 `plugin.zip`。
2. 用 MicYou 的「从文件安装插件」导入。

> **⚠️ 安装/启用后必须重启 MicYou。** `micyou-cli plugin enable` 只写状态文件，不会通知正在运行的进程重新加载插件。
> 判断是否成功：日志 `%LOCALAPPDATA%\com.lanrhyme.micyou\logs\MicYou.log` 中出现 `[dev.denlist.voicekey] VoiceKey 已启动`，且 `voicekey.dll` 被 MicYou 进程占用。

## 使用

1. 安装并重启 MicYou 后，打开插件页里的 **VoiceKey** 面板。
2. 确认「绑定按键」是对的（微信输入法默认 `Ctrl+Win`；如果你的语音快捷键不同，点「录入按键」重新录）。
3. 对着手机说话，观察电平条是否超过黄色阈值线；插件会自动按住/松开。
4. 若误触发或反应迟钝，调整「语音电平阈值」「静音判定」等参数，点「保存设置」。

## 面板参数

| 参数 | 说明 | 默认 |
| --- | --- | --- |
| `enabled` | 启用插件 | `true` |
| `keys` | 触发的组合键 | `ctrl+win` |
| `threshold` | 语音电平阈值（0–1） | `0.015` |
| `silenceMs` | 静音多久判为说完（ms） | `1000` |
| `minHoldMs` | 最短按住时长（防抖，ms） | `700` |
| `maxHoldMs` | 最长按住时长（保险，ms） | `60000` |
| `startDelayMs` | 检测到语音后延迟多久按下（ms） | `120` |
| `cooldownMs` | 两次触发之间的冷却（ms） | `800` |
| `injectMode` | 按键注入方式：`vk` 虚拟键码 / `scan` 扫描码 | `vk` |
| `onlyWhenStreaming` | 仅在音频流活动中才触发 | `true` |

## 常见问题

**Q：装了没反应？**
确认已重启 MicYou，并检查日志里是否有 `VoiceKey 已启动`。`micyou-cli` 的 enable 不会通知运行中的进程。

**Q：容易误触发 / 一直按住？**
调高 `threshold`（阈值），或调大 `startDelayMs`。噪声较大的环境建议 `threshold` 提到 `0.03` 以上。

**Q：说完了不松开？**
调小 `silenceMs`。若你常在句中停顿，可适当调大 `minHoldMs` 避免瞬断。

**Q：按键卡住不放？**
插件有 `maxHoldMs` 和「设备断开自动松开」双重保险，正常不会卡。若异常，禁用再启用插件即可复位。

**Q：Windows 下 Win 键会弹出开始菜单吗？**
不会。插件用 `SendInput` 注入的按键不会触发系统级单键行为，按住 `Ctrl+Win` 也不会弹出开始菜单。

## 技术说明

- 原生 C 插件，Win32 `SendInput` 注入按键，`kind=utility` 不进 DSP 链。
- 通过宿主提供的 `set_interval(50ms)` 轮询 `audio_state().inputLevel` 做 VAD，只读电平、不碰音频数据。
- ABI：`MPL_ABI_VERSION=1` / `MPL_API_VERSION=2`，`micyou_plugin_abi.h` 与宿主 Rust crate 布局一致。

## 用 zig 自行构建

无需 MSVC / Windows SDK，用 [zig](https://ziglang.org/) 的 `cc` 交叉编译：

```powershell
# 一键构建（自动下载 zig、编译、校验、打包）
pwsh -ExecutionPolicy Bypass -File .\scripts\build.ps1

# 或手动：
zig cc -target x86_64-windows-gnu -shared -O2 -Wall -Wextra `
    -o out\dev.denlist.voicekey\voicekey.dll src\voicekey.c -Isrc
```

冒烟测试（复刻宿主加载链 + 假宿主注入验证）：

```powershell
zig cc -target x86_64-windows-gnu -municode -O2 -o dist\loadprobe.exe test\loadprobe.c -Isrc
.\dist\loadprobe.exe            # 应输出 RESULT: PASS
```

## 目录结构

```
MicYou-VoiceKey/
├─ plugin.json          # MicYou 插件清单
├─ panel.html           # 插件配置面板
├─ src/
│  ├─ voicekey.c        # 插件主逻辑（C）
│  └─ micyou_plugin_abi.h
├─ scripts/
│  ├─ build.ps1         # 一键构建 / 校验 / 打包
│  ├─ preview.c         # 生成封面图
│  └─ savepng.c         # GDI+ PNG 写出
├─ test/
│  ├─ hostsim.c         # 假宿主 + 低级键盘钩子冒烟测试
│  └─ loadprobe.c       # 复刻 MicYou 原生加载校验链
├─ LICENSE              # Unlicense
└─ README.md
```

## 许可

本项目以 [Unlicense](LICENSE)（公有领域）发布。
