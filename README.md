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

## 软件更新与发布

- 应用内更新使用 Tauri updater 与 GitHub Release 静态 manifest。设置页可选择 `stable` 或 `beta` 通道；默认启动时自动检查，发现更新后由用户确认安装并重启。
- 固定 manifest 位于 `updater` Release：`stable.json` 指向最新正式版，`beta.json` 指向最新预发布版。
- 发布 tag 规则：`v1.2.3` 发布 stable，`v1.2.3-beta.1` 发布 beta。GitHub Actions 会构建各平台安装包并覆盖对应 manifest。
- GitHub Secrets 需配置 `TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`、`APPLE_CERTIFICATE`、`APPLE_CERTIFICATE_PASSWORD`、`APPLE_SIGNING_IDENTITY`、`APPLE_ID`、`APPLE_PASSWORD`、`APPLE_TEAM_ID`。本机验证构建可使用：

```bash
pnpm tauri signer generate --ci -p "" -w ~/.tauri/codex-relay-updater.key
TAURI_SIGNING_PRIVATE_KEY_PATH=~/.tauri/codex-relay-updater.key pnpm tauri:build
```

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

## 桌面体验与性能

- macOS 使用融合式 overlay titlebar，Windows/Linux 保留原生窗口装饰；前端通过平台 class 和安全区变量处理顶部布局，不绘制跨平台仿制窗口按钮。
- 总览保持首屏加载，档案、网关、协作和设置按页面拆包；页面数据只在进入对应入口后读取。
- UI 使用 `src/styles/global.css` 中的语义色彩、间距、圆角、阴影与动效 tokens；设置页可选择跟随系统、浅色或深色主题，主题偏好保存在当前设备，并同步到原生窗口。宽窗口侧栏可在完整导航和图标 rail 间切换，窄窗口改用浮层抽屉；`prefers-reduced-motion` 下关闭位移、缩放和 stagger 动画。
- `pnpm build` 会输出各 chunk 的原始与 gzip 大小；首屏品牌图使用独立的 128px 优化资源，原始设计图不会进入应用资源包。

## 协作机器人

Codex Relay 的“协作”页用于把手机通讯软件连接到本机 Codex。当前可配置飞书自建应用机器人、QQ 官方机器人、企业微信自建应用、Discord Bot 和 Telegram Bot：平台切换器优先展示当前配置与管理动作，完整接入步骤和命令速查收纳在“使用指南”中。保存机器人后创建“本机目录 + Codex 档案”的项目绑定，再在目标群、频道或私聊发送 `/codex bind <绑定码>` 完成会话绑定。后续可用统一 `/codex` 语义下发任务、查看会话、取消和继续 Codex 会话；Discord 优先使用已注册的 `/codex command` slash command。

常用群命令：

| 命令                                      | 用途                 |
| ----------------------------------------- | -------------------- |
| `/codex help`                             | 查看帮助             |
| `/codex projects`                         | 查看当前群已绑定项目 |
| `/codex run <project> <任务说明>`         | 启动 Codex 任务      |
| `/codex sessions [project]`               | 查看最近会话         |
| `/codex status <session_id>`              | 查看会话状态         |
| `/codex cancel <session_id>`              | 取消运行中任务       |
| `/codex continue <session_id> <追加说明>` | 继续已有 Codex 会话  |

群内下发任务需要对应平台的官方机器人权限：飞书/QQ/Discord 使用长连接或 Gateway，企业微信需要公网 HTTPS 回调 URL，Telegram 使用 Bot API 长轮询。

## 安全约定

- 所有新 Rust command 必须定义输入/输出类型、注册到 command manifest，并补充最小 capability 与测试。
- 只有确有需求时才能添加 Tauri 插件或远程 CSP 来源；权限范围必须限制到具体窗口和资源。
- 不要在前端代码、Git 历史或 `.env.example` 中提交真实密钥。
- OAuth 档案通过默认浏览器中的 PKCE 授权流程创建；完整 OAuth 凭据保存到应用数据目录中的本地加密凭据库，应用数据库只保存凭据引用和状态。
- “从 JSON 导入”使用原生多文件选择器，并在写入前完成本地结构检查和联网预检。支持官方 Codex `auth.json`（OAuth、Agent Identity、PAT）、session/token JSON、`accessToken`、`refresh_token`、`at-…` / `personal_access_token`，以及 Sub2API 的 `accounts[].credentials` OpenAI OAuth 导出；不读取 cookie 或 `session_token`，不会对未知 JSON 递归猜测。预览只显示脱敏身份、来源、认证方式和验证结果，用户可逐项选择；未验证、无效项默认不可导入，批量提交允许局部成功。
- JSON 导入的原始凭据只在 Rust 内存预览会话（15 分钟）与本地加密凭据库中出现，不会写入 SQLite、IPC 返回值或日志。预检使用私有权限、原子写入的临时 `CODEX_HOME/auth.json`，验证后立即删除。导入更新已有 OAuth 档案时，access-token-only 输入不会覆盖已保存的 refresh token。
- 档案页将 OAuth 与 JSON 导入分为独立入口卡片；所有 Codex 档案均显示邮箱、认证方式和身份状态。预检得到的邮箱、账号 ID 与套餐摘要会在提交时缓存为非敏感档案资料；账号 ID 默认掩码并放在可展开详情中。OAuth、PAT 与 Agent Identity 都会使用官方 Codex app-server 尝试同步额度；上游不返回额度时会保留最近成功数据并显示可手动重试的状态。
- 切换当前 Codex 档案时，Relay 会按认证模式生成官方兼容的 `.codex/auth.json` 并原子写入当前用户默认路径。设置页可选择三种桌面工作区模式：每次全新启动（每次创建并保留新的空白工作区）、账号独立工作区（每个档案复用自己的 Electron 数据目录）或共享原客户端状态（不传入 Electron 数据目录，复用原 ChatGPT/Codex 客户端）。共享模式会在切换前请求正常关闭客户端；若无法关闭，不会修改当前档案或凭据。macOS 还会写入该档案 `CODEX_HOME` 对应的 `Codex Auth` Keychain 条目。
- macOS 构建使用固定的开发签名身份。升级前由临时签名保存的 OAuth 档案不会在启动时探测旧 Keychain 条目，以免触发系统密码框；这些档案会显示为需要重新授权，完成一次 OAuth 后即可正常切换。
- Relay 自身的 OAuth、API Key、Client Key 和协作机器人密钥保存于应用数据目录的 `secrets.key` + `secrets.vault` 本地加密凭据库，不再使用 `com.codexrelay.app` 登录钥匙串条目；升级到本地凭据库后，旧 Keychain 凭据不会弹窗迁移，相关档案需要重新授权或重新录入。
- 所有已配置 Codex 档案的“刷新资料”都会优先通过本机 `codex app-server` 读取账户资料、ChatGPT/Codex 套餐与额度窗口（剩余百分比、窗口时长和重置时间）。档案页首次进入、回到前台、任务结束及页面可见期间每 30 秒都会后台更新；结果作为非敏感摘要缓存到本机数据库，单个账号失败会保留最近成功数据并标记过期。OAuth 凭据仍只保存在本地加密凭据库，不会通过 IPC 返回。
- 后台同步直接读取本地加密凭据库，并优先复用 OAuth 的进程内缓存；不会触发 macOS 登录钥匙串授权弹窗。刷新后的 OAuth 凭据会写回本地加密凭据库。
- 当官方响应未包含额度或订阅周期结束时间时，Relay 会仅向 OpenAI 的 `chatgpt.com` 兼容接口发送该档案的 OAuth access token，以读取额度或 entitlement 摘要；不会读取浏览器 Cookie 或聊天内容。该后备接口不是稳定公开 API，字段缺失或失败时会显示“上游未提供”而不会推算数据。
- OAuth 档案默认不加入网关账号池。用户刷新可用模型并显式加入后，Relay 可通过 Codex Responses 上游代发 `/v1/responses`，并为 `/v1/chat/completions` 转换文本与 function tools；凭据仍只在本地加密凭据库和短生命周期内存中出现。Codex 网关切换中的“OAuth 登录档案”只允许绑定通过 OAuth 授权流程创建或更新的档案；JSON 导入账号即使是 OAuth token，也仅用于反代账号池，不用于登录态解锁。路由先按优先级与额度可用性分层，再执行平滑加权轮换；401 会刷新一次凭据，429、5xx 和网络错误会触发冷却与首字节前故障切换。该适配依赖当前 Codex 产品协议，与 OpenAI Platform API Key 认证相互独立。

## 质量与协作

提交前的 Husky 钩子会运行暂存文件检查，提交信息遵循 Conventional Commits，例如 `feat: add settings view`。完整贡献流程见 [CONTRIBUTING.md](CONTRIBUTING.md)。
