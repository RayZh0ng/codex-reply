# Contributing

## 开发流程

1. 使用 Node 24、pnpm 10 和 Rust stable 安装依赖：`pnpm install`。
2. 修改前端时运行 `pnpm test`、`pnpm lint` 和 `pnpm typecheck`；修改 Rust 时运行对应的 `pnpm rust:*` 命令。
3. 提交前必须通过 `pnpm check`；修改 Tauri 配置、能力或打包逻辑时，再运行 `pnpm tauri:build`。
4. 使用 Conventional Commits，例如 `feat: add settings view`、`fix: handle empty state`。

## Tauri 安全变更

- 新增前端可调用的 Rust command 时，先定义参数和返回类型，再注册 command manifest，并为成功和失败场景编写测试。
- 按窗口和插件细分 `src-tauri/capabilities/`；不得以默认全局授权替代具体权限。
- 新增远程请求、文件访问、Shell 或插件时，更新 CSP、权限说明和 README，并在 PR 中说明最小化范围。
