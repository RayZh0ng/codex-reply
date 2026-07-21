# Codex Relay

一个面向团队协作的 Tauri 2 桌面应用起始项目。前端使用 React、TypeScript 与 Vite，桌面端使用 Rust；默认不开放文件系统、Shell、外部网络或业务 IPC 权限。

## 前置条件

- Node.js 24（见 [`.nvmrc`](.nvmrc)）与 pnpm 10
- Rust stable（包含 `rustfmt`、`clippy`）
- macOS：Xcode Command Line Tools

## 快速开始

```bash
corepack enable
pnpm install
pnpm tauri:dev
```

## 常用命令

| 命令               | 用途                                           |
| ------------------ | ---------------------------------------------- |
| `pnpm dev`         | 仅启动 Vite 前端开发服务器                     |
| `pnpm tauri:dev`   | 启动完整 Tauri 桌面应用                        |
| `pnpm build`       | 类型检查并构建前端资源                         |
| `pnpm tauri:build` | 生成 macOS debug `.app` 包                     |
| `pnpm check`       | 执行格式、Lint、类型、前端测试与 Rust 质量检查 |
| `pnpm format`      | 格式化可编辑文件                               |

## 目录

```text
src/
  app/       应用入口与全局页面壳
  features/  按业务领域划分的功能模块
  shared/    稳定复用的组件、工具与类型
  styles/    全局样式和设计基础
src-tauri/
  capabilities/  Tauri 窗口权限
  src/           Rust 运行时入口
```

## 安全约定

- 所有新 Rust command 必须定义输入/输出类型、注册到 command manifest，并补充最小 capability 与测试。
- 只有确有需求时才能添加 Tauri 插件或远程 CSP 来源；权限范围必须限制到具体窗口和资源。
- 不要在前端代码、Git 历史或 `.env.example` 中提交真实密钥。

## 质量与协作

提交前的 Husky 钩子会运行暂存文件检查，提交信息遵循 Conventional Commits，例如 `feat: add settings view`。完整贡献流程见 [CONTRIBUTING.md](CONTRIBUTING.md)。
