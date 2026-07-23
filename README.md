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

| 命令                 | 用途                                           |
| -------------------- | ---------------------------------------------- |
| `pnpm dev`           | 仅启动 Vite 前端开发服务器                     |
| `pnpm tauri:dev`     | 启动完整 Tauri 桌面应用                        |
| `pnpm build`         | 类型检查并构建前端资源                         |
| `pnpm tauri:build`   | 生成当前操作系统的 Tauri debug 安装包          |
| `pnpm tauri:package` | 生成已签名的日常使用 macOS `.app` 包           |
| `pnpm check`         | 执行格式、Lint、类型、前端测试与 Rust 质量检查 |
| `pnpm format`        | 格式化可编辑文件                               |

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
- OAuth 档案通过默认浏览器中的 PKCE 授权流程创建；完整 OAuth 凭据保存到系统 Keyring，应用数据库只保存凭据引用和状态。
- 切换当前 OAuth 档案时，Relay 会按需刷新已保存的凭据，并原子写入当前用户默认 `.codex/auth.json`。设置页可选择三种桌面工作区模式：每次全新启动（每次创建并保留新的空白工作区）、账号独立工作区（每个档案复用自己的 Electron 数据目录）或共享原客户端状态（不传入 Electron 数据目录，复用原 ChatGPT/Codex 客户端）。共享模式会在切换前请求正常关闭客户端；若无法关闭，不会修改当前档案或凭据。macOS 还会写入该档案 `CODEX_HOME` 对应的 `Codex Auth` Keychain 条目。
- macOS 构建使用固定的开发签名身份。升级前由临时签名保存的 OAuth 档案不会在启动时探测旧 Keychain 条目，以免触发系统密码框；这些档案会显示为需要重新授权，完成一次 OAuth 后即可正常切换。
- 日常使用请从 `pnpm tauri:package` 生成的 `Codex Relay.app` 启动，而不是 `pnpm tauri:dev` 的临时签名二进制。稳定签名版与 Keychain 服务统一使用 `com.codexrelay.app`；首次主动账号操作时可在 macOS 弹窗中选择“始终允许”。
- OAuth 档案的“同步资料”会优先通过本机 `codex app-server` 读取账户资料、ChatGPT/Codex 套餐与额度窗口（剩余百分比、窗口时长和重置时间）。应用启动、回到前台和每分钟都会后台更新；结果作为非敏感摘要缓存到本机数据库，单个账号失败会保留最近成功数据并标记过期。OAuth 凭据仍只保存在系统 Keyring，不会通过 IPC 返回。
- 后台同步只会以禁止 Keychain UI 的方式读取或写入 OAuth 凭据，并优先复用进程内缓存；若系统需要授权，不会弹出密码框，账号卡片会提示“解锁并同步”。用户主动同步、切换账号、恢复工作区或启动受管任务才会请求钥匙串授权。刷新后的凭据若暂时无法静默写回，会仅保留在当前会话并于下一次用户主动操作时保存。
- 当官方响应未包含额度或订阅周期结束时间时，Relay 会仅向 OpenAI 的 `chatgpt.com` 兼容接口发送该档案的 OAuth access token，以读取额度或 entitlement 摘要；不会读取浏览器 Cookie 或聊天内容。该后备接口不是稳定公开 API，字段缺失或失败时会显示“上游未提供”而不会推算数据。

## 质量与协作

提交前的 Husky 钩子会运行暂存文件检查，提交信息遵循 Conventional Commits，例如 `feat: add settings view`。完整贡献流程见 [CONTRIBUTING.md](CONTRIBUTING.md)。
