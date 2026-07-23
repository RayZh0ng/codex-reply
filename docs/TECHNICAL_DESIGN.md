# Codex Relay 技术设计（MVP）

## 1. 架构概览

Codex Relay 是本机优先的 Tauri 2 桌面应用。React 前端通过 Tauri IPC 调用 Rust command；Rust 负责账号数据、网关监听、上游请求、Codex 运行时和 Webhook 投递。

```text
React UI --Tauri IPC--> Domain services --+--> SQLite (profiles, settings, metrics)
                                            +--> Account storage
                                            +--> Codex runtime and desktop apps
                                            +--> Gateway and notification clients
```

## 2. 模块边界

| 模块            | 职责                                                        |
| --------------- | ----------------------------------------------------------- |
| `domain`        | 档案、账号池、网关、通知、指标的输入/输出类型。             |
| `database`      | SQLite schema、migration 与仓储。                           |
| `profiles`      | 档案创建、更新、删除、当前档案选择和账号池配置。            |
| `secrets`       | 账号凭据、密钥生成和引用管理。                              |
| `gateway`       | HTTP(S) 监听、路由、SSE/HTTP 透传和运行统计。               |
| `notifications` | 事件信封、频道投递、重试与测试投递。                        |
| `codex_runtime` | `CODEX_HOME`、OAuth 登录、认证投影、桌面端与 CLI 进程管理。 |
| `observability` | 聚合指标、状态快照和事件发布。                              |
| `commands`      | Tauri command 的输入验证、流程编排和 DTO 映射。             |

## 3. 数据模型

SQLite 位于应用数据目录，使用 migration 管理：

- `profiles`：ID、别名、来源类型、连接数据、模型、状态、账号池策略与当前档案标记。
- `client_keys`：ID、名称、值、创建/最后使用/撤销时间。
- `channels`：类型、启用状态、端点、签名信息和最后结果。
- `metrics_buckets`：时间桶、总请求、成功/失败、延迟和估算 token 数。

OAuth token、API Key 与其他机密仅保存于系统 Keychain；SQLite 中的档案索引只保存凭据引用、非敏感摘要和当前账号标记。

## 4. IPC 与前端

React 功能目录通过 `shared/ipc.ts` 调用 Rust command。前端状态包括仪表盘、档案、网关和通知数据；新增 command 先定义 Rust DTO，再定义前端类型和调用封装。

常用 command：

| 领域  | command                                                                                         |
| ----- | ----------------------------------------------------------------------------------------------- |
| 档案  | `list_profiles`、`create_profile`、`update_profile`、`delete_profile`、`select_current_profile` |
| OAuth | `start_oauth_import`、`oauth_import_status`、`complete_oauth_import`                            |
| 任务  | `start_managed_task`、`managed_task_status`、`cancel_managed_task`                              |
| 网关  | `gateway_status`、`update_gateway`、`start_gateway`、`stop_gateway`                             |
| 通知  | `upsert_channel`、`test_channel`、`delete_channel`                                              |

## 5. 网关与账号池

网关以 `axum`/`tokio` 提供 `/healthz`、`/v1/models`、`/v1/responses` 和 `/v1/chat/completions`，使用 `reqwest` 访问上游。

路由先筛选启用、在池内且支持模型的档案，再按优先级和权重选择成员。首个响应字节前发生临时失败时，可将成员置为冷却并选择下一成员重试；响应开始后保持当前成员。

## 6. Codex 档案与运行时

每个档案可拥有独立 `CODEX_HOME`、认证数据和运行目录。OAuth 流程启动浏览器或内置窗口、接收回调并保存 token；OAuth token 过期时可使用 refresh token 更新。OAuth token 仅驻留系统 Keychain 与当前进程的短生命周期缓存，绝不写入 SQLite、IPC 或日志。后台额度刷新使用禁止 Keychain UI 的读取/写入路径；钥匙串锁定或尚未授权时立即以缓存摘要降级并标记可由用户“解锁并同步”，不会显示 macOS 密码框。用户主动同步、切换、恢复或启动受管任务才允许读取系统 Keychain；刷新后的凭据若无法静默持久化，保留在当前会话并等待下一次主动操作保存。macOS 打包应用的 Bundle ID 与 Keychain 服务统一为 `com.codexrelay.app`，日常使用应从稳定签名的 `Codex Relay.app` 启动，而非开发态临时签名进程。

切换当前档案时，应用加载并更新目标档案的认证数据，原子投影到当前用户默认 `.codex/auth.json`，并写入档案专属 `CODEX_HOME`。桌面工作区模式保存在 `app_settings.desktop_workspace_mode`，缺省为 `per_profile`：`fresh` 为每次切换生成并记录一个新的受控 Electron 数据目录；`per_profile` 为每个档案复用稳定目录；`shared` 不传入 `CODEX_ELECTRON_USER_DATA_PATH` 或 `--user-data-dir`，使用原客户端默认目录。共享模式必须由用户确认，后端先请求正常退出 ChatGPT/Codex 并等待；失败时不写入新凭据或当前档案，绝不强制结束进程。macOS 通过 LaunchServices 的 `open -n -a` 传入 `CODEX_HOME`，隔离模式额外传入用户数据目录；Windows 使用等价环境变量和参数。全新工作区仅保存 Relay 的 ID、所属档案与时间元数据，历史记录可恢复或显式删除。档案资料同步通过 `codex app-server` 的 `account/read` 与 `account/rateLimits/read` 读取套餐和 ChatGPT 额度窗口，并把非敏感摘要缓存到 SQLite；官方响应缺失订阅周期或额度时，仅向 OpenAI `chatgpt.com` 的兼容端点查询 entitlement 或 usage 摘要。同步在启动、前台恢复和每分钟执行，最多并发三个档案；每个档案的额度和订阅状态独立保留最近成功值。受管任务以目标档案的 `CODEX_HOME` 启动 `codex exec` 子进程。

## 7. 通知

Rust 侧支持飞书机器人、企业微信机器人和自定义 HTTPS `POST`。通知使用 PRD 定义的事件信封；每个频道保存投递结果并可重试。

## 8. 实施顺序

1. 建立 SQLite migration、账号存储和 `Masked*` DTO。
2. 实现档案、账号池、健康/冷却与聚合指标服务。
3. 实现网关、Responses/Chat 路由与合约测试。
4. 实现 Codex OAuth、认证投影、运行时和桌面端切换。
5. 实现通知投递和 React 功能页。
6. 每阶段运行 `pnpm test`、`pnpm lint`、`pnpm typecheck`、`pnpm rust:format`、`pnpm rust:lint`、`pnpm rust:test`；新增 capability/CSP 或打包变更后运行 `pnpm tauri:build`，交付前运行 `pnpm check` 与 `git diff --check`。

## 9. 待确认

- Sub2API JSON schema、OAuth 提供方和第三方兼容矩阵。
- 首批第三方中转站、模型映射和 API 兼容性测试矩阵。
- 局域网部署的目标操作系统、地址策略和管理员责任边界。
