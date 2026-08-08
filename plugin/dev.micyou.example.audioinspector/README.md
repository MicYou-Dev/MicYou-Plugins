# Audio Inspector 音频监视器（标准示例插件）

演示标准插件写法的示例：定时采样宿主音频状态与设备列表并在面板实时显示

## 功能

- 每 2 秒通过 `set_interval` 采样 `audio_state` 与 `connected_devices`
- 状态写入 config（`set_config`），面板轮询 `get_config` 实时显示
- 侧边栏图标 `set_panel_icon`（📊）
- 面板文案跟随宿主语言（`locale` API，中/英）
- 手动「刷新」按钮（`ui:refresh` 动作 → `handle_message`）

## 能力声明

```json
"capabilities": ["config.read", "config.write", "audio.state", "device.list"]
```

## 构建

```bash
cargo build --release
# 产物 target/release/libmicyou_example_audio_inspector.so
```

## 安装

- 应用「插件」页 → 导入 `plugin.zip`
- 或解压到 `~/.config/micyou/plugins/dev.micyou.example.audioinspector/`
