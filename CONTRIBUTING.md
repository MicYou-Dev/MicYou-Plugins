# 贡献插件到 MicYou 插件市场

## 提交流程

1. 在 `plugin/<插件ID>/` 下创建目录（ID 用反向域名，如 `dev.micyou.example.soundpad`）
2. 放置 `plugin.json`（manifest，字段参考 [插件开发指南](https://micyou.top/docs/plugin/plugin-development-guide)）
3. 用 micyou-cli 打包：`micyou plugin package <插件目录> -o plugin.zip`，放入同目录
4. 可选：`README.md` 插件说明、`preview.png` 预览图（建议 640×360）
5. 提交 PR，CI 自动重新生成 `index.json` 与 README 表格

## 要求

- manifest 校验通过（`micyou plugin validate`）
- 插件源码开源（市场收录要求），仓库地址写入 `repository` 字段
- 能力声明与实际调用一致（宿主按声明授权）
- 更新版本时同步提升 manifest 中的 `version`（semver）
