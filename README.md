# Codex Relay

Codex Relay 是一个本机优先的 Tauri 2 桌面应用，用来管理 Codex 档案、聚合多账号 API 网关，并把群聊/频道中的协作请求转发到本机 Codex CLI。

当前版本：`v0.2.0-beta.1`。本仓库采用 MIT License 开源。

## 功能概览

- **Codex 档案管理**：管理 OAuth、PAT、Agent Identity 与 OpenAI-compatible/API 服务档案，缓存账号资料、额度与可用模型。
- **本机 API 网关**：提供 OpenAI Responses、Chat Completions、Anthropic Messages、Gemini 和 Ollama 兼容入口，支持账号池、模型映射、冷却与 Client Key。
- **协作机器人**：飞书、QQ、企业微信、Discord、Telegram 统一 `/codex` 命令，把群消息转换为本机 Codex 任务。
- **项目共享上下文**：同一工作目录 + 执行方式 + 档案/模型复用一个稳定 `CODEX_HOME`、active Codex session、长期记忆和目标状态。
- **桌面更新**：Tauri updater 支持 stable/beta 通道，GitHub Release 发布 manifest。

## 快速开始

```bash
corepack enable
pnpm install
pnpm tauri:dev
```

要求：Node.js 24、pnpm 10、Rust stable（含 `rustfmt`、`clippy`），macOS 需要 Xcode Command Line Tools。

常用开发命令：

| 命令                 | 用途                                       |
| -------------------- | ------------------------------------------ |
| `pnpm dev`           | 启动 Vite 前端开发服务器                   |
| `pnpm tauri:dev`     | 启动完整桌面应用                           |
| `pnpm build`         | 类型检查并构建前端资源                     |
| `pnpm tauri:build`   | 生成当前平台 Tauri debug 安装包            |
| `pnpm tauri:package` | 生成日常发布安装包                         |
| `pnpm check`         | 运行格式、lint、类型、前端测试与 Rust 检查 |

## 协作机器人

在“协作”页创建机器人，再创建项目绑定：项目名、群命令 slug、本机工作目录、执行方式（档案直连或 API 网关）、档案/模型。保存后在目标群、频道或私聊发送：

```text
/codex bind <绑定码>
```

绑定完成后，首次 @ 机器人会创建新的项目共享上下文；之后自然对话会续接同一 Codex context，除非显式创建新会话。

### 命令速查

| 命令                                                | 行为                                                                     |
| --------------------------------------------------- | ------------------------------------------------------------------------ |
| `@机器人 <自然语言>`                                | 首次创建项目共享上下文，后续续接 active Codex session                    |
| `/codex new [project] <task>` / `新会话` / `新任务` | 创建并切换新的 Codex 会话                                                |
| `/codex plan <task>`                                | 以 Relay 计划模式提示词执行一个 turn                                     |
| `/codex goal <objective>`                           | 设置长期目标并注入后续 turn                                              |
| `/codex goal edit <objective>`                      | 修改长期目标                                                             |
| `/codex goal pause` / `resume` / `clear`            | 暂停、恢复或清空长期目标                                                 |
| `/codex memories on` / `off` / `status`             | 更新或查看 context memory 配置                                           |
| `/codex model [model]`                              | 查看或切换上下文默认模型                                                 |
| `/codex permissions [policy]`                       | 查看或切换权限策略：`read-only`、`workspace-write`、`danger-full-access` |
| `/codex status [session_id]`                        | 查看 context 状态或指定会话状态                                          |
| `/codex resume <session_id>`                        | 切换 active Codex session                                                |
| `/codex compact`                                    | 压缩当前上下文供后续续接                                                 |
| `/codex review`                                     | 审查当前项目改动                                                         |
| `/codex sessions [project]`                         | 查看最近 Relay 会话                                                      |
| `/codex cancel <session_id>`                        | 取消运行中的 turn                                                        |
| `/codex continue <session_id> <task>`               | 兼容旧式按会话继续                                                       |

Discord 使用已注册的 `/codex command` slash command，例如 `command: "memories status"`。

## 长期记忆与上下文模型

`v0.2.0-beta.1` 引入两个本地表：

- `collaboration_contexts`：记录项目全局 scope、稳定 `CODEX_HOME`、active Codex session、memory、goal、model 与 permissions 快照。
- `collaboration_chat_state`：记录每个群/频道/私聊当前使用的项目上下文。

每次 Codex 运行只把附件和 `last-message.txt` 放进 per-run 目录；`CODEX_HOME/config.toml` 会按 context 写入：

```toml
[features]
memories = true

[memories]
use_memories = true
generate_memories = true
```

同一 context 同时只允许一个 Codex turn 运行；运行中消息会提示查看 status 或取消。

## API 网关

网关可在 `127.0.0.1` 与局域网私有地址之间切换，使用 Relay Client Key 鉴权，并可用 CIDR allowlist 收窄来源。

公开入口：

| 接口                                    | 行为                                             |
| --------------------------------------- | ------------------------------------------------ |
| `GET /healthz`                          | 健康、证书、冷却、Client Key 和模型摘要          |
| `GET /v1/models`                        | 返回账号池可见模型                               |
| `POST /v1/responses`                    | OpenAI Responses 兼容入口                        |
| `POST /v1/chat/completions`             | OpenAI Chat Completions 兼容入口，支持 SSE/tools |
| `POST /v1/messages`                     | Anthropic Messages 入口                          |
| `POST /v1beta/*`                        | Gemini generateContent/streamGenerateContent     |
| `POST /api/chat` / `POST /api/generate` | Ollama 兼容入口                                  |

## 配置与密钥安全

- OAuth、API Key、Client Key、机器人 Secret 存在应用数据目录的本地加密 vault 中。
- SQLite 与 IPC 只返回脱敏信息或 secret reference。
- JSON 导入预览只保留短生命周期内存状态，提交前不会写入 SQLite。
- 暴露 LAN、隧道或公网回调 URL 时，请限制 CIDR、轮换 Client Key，并避免在日志/Issue 中提交真实 secret。

## 项目结构

```text
src/
  app/       应用入口、页面壳与全局装配
  features/  按业务领域组织的 UI、状态与测试
  shared/    跨功能稳定复用的 IPC、UI、主题和工具
  styles/    全局样式与设计 tokens
src-tauri/
  capabilities/  Tauri 权限配置
  src/           Rust runtime、数据库、网关、协作连接器
```

## 开发检查

按改动范围运行：

```bash
pnpm test
pnpm lint
pnpm typecheck
pnpm rust:format
pnpm rust:lint
pnpm rust:test
pnpm tauri:build
pnpm check
git diff --check
```

提交信息遵循 Conventional Commits，例如：

```bash
git commit -m "feat: add collaboration memory contexts"
```

## 发布 beta

本仓库的 release workflow 监听 `v*` tag：

```bash
git tag v0.2.0-beta.1
git push origin codex/collaboration-memory-beta
git push origin v0.2.0-beta.1
```

`vX.Y.Z-beta.N` 会生成 GitHub prerelease，并覆盖 updater Release 中的 `beta.json` manifest；正式版 `vX.Y.Z` 覆盖 `stable.json`。

本地构建 updater 包可使用：

```bash
pnpm tauri signer generate --ci -p "" -w ~/.tauri/codex-relay-updater.key
TAURI_SIGNING_PRIVATE_KEY_PATH=~/.tauri/codex-relay-updater.key pnpm tauri:build
```

## 贡献

欢迎 Issue、PR 与安全报告。请先阅读 [CONTRIBUTING.md](CONTRIBUTING.md) 与 [SECURITY.md](SECURITY.md)，提交前运行对应检查并保留验证结果。
