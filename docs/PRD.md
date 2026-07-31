# Codex Relay 产品需求文档（MVP）

| 项目     | 内容                        |
| -------- | --------------------------- |
| 状态     | Draft                       |
| 版本     | 0.2 beta                    |
| 更新日期 | 2026-07-29                  |
| 产品形态 | 本机优先的 Tauri 2 桌面应用 |

## 1. 背景与目标

Codex Relay 帮助个人开发者和小团队在一台本机上管理多个 Codex 档案，将可用档案组成账号池，并通过协作入口在群聊中创建和管理 Codex 任务。

### 1.1 MVP 目标

1. 用户可添加、查看、切换、启停和删除多个 Codex 档案。
2. 用户可将档案组成账号池，并向客户端提供 OpenAI 兼容 API。
3. 用户可将本机项目绑定到协作机器人，并在群聊中启动、查看、取消和继续 Codex 任务。

### 1.2 非目标

- 不在 MVP 中实现账单或配额购买。
- 不承诺与所有第三方中转站完全兼容。

## 2. 用户、术语与范围

### 2.1 目标用户

- 个人开发者：需要在多个 Codex 账号之间快速切换并在本机使用。
- 小型团队管理员：需要在单台机器上管理账号池、API 服务和群聊协作任务入口。

### 2.2 核心术语

| 术语       | 定义                                                   |
| ---------- | ------------------------------------------------------ |
| 档案       | 一套账号、连接、能力、状态与凭据数据。                 |
| 凭据       | OAuth token、API Key、App Secret 等账号数据。          |
| 账号池     | 加入网关、启用且可用的档案集合。                       |
| 当前档案   | 用户为本机 Codex 操作选择的档案。                      |
| 客户端 Key | 客户端调用网关时使用的访问标识。                       |
| 协作连接器 | 飞书、QQ、企业微信、Discord、Telegram 等群聊软件入口。 |

### 2.3 MVP 范围

| 领域     | MVP 提供                                                                                 |
| -------- | ---------------------------------------------------------------------------------------- |
| 多账号   | 手动注册、多格式 JSON 批量导入、OAuth、第三方中转站 API Key、切换与账号池管理            |
| 网关     | `/v1/models`、`/v1/responses`、`/v1/chat/completions`、流式与非流式响应、健康检查        |
| 协作     | 飞书、QQ、企业微信、Discord、Telegram 官方机器人连接器、群项目绑定、群命令与会话状态回传 |
| 可观测性 | 网关请求统计、延迟、成功率、健康和冷却状态                                               |

## 3. 产品体验与信息架构

桌面端使用“服务状态 + 统计卡片 + 账号卡片/列表”的信息层级。

### 3.1 概览

- 展示当前档案、网关运行状态、账号池成员数、冷却成员数和协作入口状态。
- 展示本产品采集的总请求数、成功/失败数、平均延迟和估算 token 数。

### 3.2 档案与账号池

- 支持卡片和列表视图、搜索、标签、启用/停用、健康检查和当前档案标识。
- 档案详情展示别名、来源类型、模型能力、最近验证时间、健康/冷却状态和错误摘要。
- 用户可为档案设定网关优先级、权重和是否加入账号池；当前档案切换不改变账号池成员资格。
- OAuth 档案默认不加入账号池；仅认证模式为 OAuth、已刷新模型且由用户显式加入的档案可用于 Codex Responses 代发。PAT 与 Agent Identity 仍只用于本机 Codex 运行时。

### 3.3 网关服务

- 服务控制页提供启动/停止、监听状态、loopback/LAN 监听模式、绑定地址、端口、CIDR、客户端 Key 管理和健康摘要。
- 已生成的客户端 Key 提供创建、查看、轮换和撤销能力。

### 3.4 协作入口

- 侧边栏提供“协作”入口，作为飞书、QQ、企业微信、Discord、Telegram 等通讯软件连接 Codex 的统一位置。
- 当前版本五个平台均可配置：飞书自建应用机器人、QQ 官方机器人、企业微信自建应用、Discord Bot 和 Telegram Bot。
- 协作页以紧凑平台切换器和当前状态为主；未配置时显示短引导，完整创建、权限、群绑定和命令说明收纳在“使用指南”中。

### 3.5 多平台协作机器人

- 支持配置飞书自建应用机器人、QQ 官方机器人、企业微信自建应用、Discord Bot 和 Telegram Bot。
- 飞书、QQ、Discord 使用长连接/Gateway 接收事件；Telegram 使用 Bot API 长轮询；企业微信使用自建应用回调 URL。
- 支持在客户端为任一机器人绑定本机项目目录与 Codex 档案，并生成群绑定码。
- 支持在群、频道或私聊中使用统一 `/codex` 命令启动、查看、取消和继续 Codex 会话；Discord 优先使用 `/codex command` slash command。

## 4. 功能需求

### 4.1 档案导入与切换

| 编号   | 需求                                                       | 验收标准                                                                                                                                                                                                                                                                                                                                              |
| ------ | ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ACC-01 | 用户可手动新建档案，填写别名、来源类型、连接元数据和凭据。 | 保存后可在档案列表中查看并编辑。                                                                                                                                                                                                                                                                                                                      |
| ACC-02 | 支持多格式 JSON 文件批量导入档案。                         | 原生文件选择器可选择最多 100 个 `.json` 文件；支持 Codex `auth.json`（OAuth、Agent Identity、PAT）、明确 token/session JSON、仅 `accessToken`、`refresh_token`、`at-…` / `personal_access_token` 与 Sub2API `accounts[].credentials` 中的 OpenAI OAuth 项。解析后必须显示脱敏预览、联网预检结果与逐项勾选；无效或未验证项默认不可选，提交可局部成功。 |
| ACC-03 | 支持 OAuth 授权。                                          | 授权完成后创建或更新档案并保存凭据。                                                                                                                                                                                                                                                                                                                  |
| ACC-04 | 支持 OpenAI 兼容第三方中转站的 API Key 连接。              | 保存后可用于网关路由。                                                                                                                                                                                                                                                                                                                                |
| ACC-05 | 用户可切换当前档案。                                       | 已保存的目标凭据直接原子写入当前用户的 `.codex/auth.json`。切换前用户确认正常关闭并重启 Codex/ChatGPT 客户端；启动时不传入受控 Electron 用户数据目录，固定复用原客户端本机状态以保留聊天记录、记忆和设置。不写入 macOS `Codex Auth` Keychain，避免账号切换触发系统钥匙串授权弹窗。无需再次授权。                                                      |
| ACC-06 | 用户可启用、停用、删除、标记和配置账号池成员。             | 档案状态变化同步到账号池。                                                                                                                                                                                                                                                                                                                            |

> 导入仅接受明确的认证字段和当前 Sub2API `accounts[].credentials` 的 `platform: "openai"`、`type: "oauth"` 项；不支持浏览器 cookie、`session_token` 或未知 JSON 的递归猜测。

> 档案页把官方 OAuth 与 JSON 导入作为独立入口展示。所有 Codex 档案均尝试通过官方 Codex app-server 读取套餐与额度；账号 ID 仅以掩码形式出现在可展开详情中。上游未返回额度时必须显示可重试的明确状态，不得估算或伪造结果。

### 4.2 账号池与路由

- 已启用、已加入账号池且声明支持所请求模型的档案可参与路由。
- 先按优先级选择候选集合，再优先使用额度新鲜且未耗尽的成员，并在同一额度层内执行平滑加权轮换；额度缺失或过期成员作为后备。
- 上游临时失败时，成员进入冷却，并可切换到下一合格成员重试。
- OAuth 上游 401 时刷新凭据并重试当前成员一次；429、5xx 与网络错误只允许在响应首字节前切换成员。Responses 的 `previous_response_id` 必须保持原档案亲和。
- API Key 上游 401/403 时标记档案 unhealthy 并尝试下一候选；429、5xx 与首字节前超时进入冷却并尝试下一候选。Dispatcher 必须遍历全部合格候选；SSE 已输出首字节后中断时不再切换成员，而是输出对应失败事件并记录最近上游错误。
- JSON 导入的 OAuth 账号可作为反代账号池成员参与路由，但不得作为 Codex 网关切换中的 OAuth 登录态解锁档案；相关下拉项必须禁用并说明原因。
- 无本地模型返回 404；候选全部处于冷却/额度耗尽返回 503；全部上游候选失败返回 502。

### 4.3 网关 API

| 接口                        | 行为                                                                                                 |
| --------------------------- | ---------------------------------------------------------------------------------------------------- |
| `GET /healthz`              | 返回 `status`、运行状态、冷却数、证书状态、Client Key 数、provider/surface 模型聚合和最近上游错误。  |
| `GET /v1/models`            | 返回账号池可提供的模型标识。                                                                         |
| `POST /v1/responses`        | 接受 OpenAI Responses 字段，路由到合格成员；OpenAI direct 透传，Chat/Provider 上游通过适配层转换。   |
| `POST /v1/chat/completions` | 接受文本、图片、function tools/tool results，支持流式与非流式；adapter provider 支持 `n>1` fan-out。 |

兼容层要求：

- OpenAI / OpenAI-compatible direct 在 wire API 匹配时完整透传，并将响应中的上游模型名回写为用户请求的可见模型名。
- Codex OAuth 通过 Responses 上游服务 `/v1/responses`，并与 `/v1/chat/completions` 互转文本、function tools、usage 和 SSE 事件。
- Anthropic 通过 Messages API 映射 system/messages/content blocks、function tools、tool_use/tool_result、usage 与 Messages SSE。
- Gemini 使用当前 v1beta `generateContent` / `streamGenerateContent` 路径，映射 `contents/parts`、`systemInstruction`、function declarations、JSON schema、usageMetadata 和 SSE。
- Ollama 的 OpenAI Chat/Responses 统一落到 `/api/chat`，保留 native `/api/generate`；映射 `tools`、`format`、`options`、`think`、usage 和 newline JSON streaming。
- 非 OpenAI adapter 对没有等价能力的 `audio`、`logprobs`、`top_logprobs` 在请求前返回 OpenAI 风格 `unsupported_parameter`。

### 4.4 协作命令、上下文与卡片回传

v0.2 beta 提供五个平台通用的协作命令、项目共享上下文和会话状态回传。首次 @ 机器人时创建项目共享上下文；后续自然对话复用同一 active Codex session；只有显式 `/codex new`、`新会话`、`新任务` 创建并切换新会话。

| 命令                                      | 行为                                       |
| ----------------------------------------- | ------------------------------------------ |
| `/codex help`                             | 查看帮助                                   |
| `/codex bind <code>`                      | 将项目绑定到当前群                         |
| `/codex projects`                         | 查看当前群已绑定项目                       |
| `@机器人 <自然语言>`                      | 首次创建项目共享上下文，后续续接同一上下文 |
| `/codex new [project] <任务说明>`         | 创建并切换新的 Codex 会话                  |
| `/codex plan <任务说明>`                  | 以计划模式提示词执行一个 turn              |
| `/codex goal <objective                   | edit                                       | pause   | resume                           | clear>` | 持久化并管理长期目标 |
| `/codex memories on                       | off                                        | status` | 更新或查看 context memories 配置 |
| `/codex model [model]`                    | 查看或切换上下文默认模型                   |
| `/codex permissions [policy]`             | 查看或切换权限策略                         |
| `/codex status [session_id]`              | 查看上下文或指定会话状态                   |
| `/codex resume <session_id>`              | 切换 active Codex session                  |
| `/codex compact`                          | 压缩当前上下文                             |
| `/codex review`                           | 审查当前项目改动                           |
| `/codex sessions [project]`               | 查看最近会话                               |
| `/codex cancel <session_id>`              | 取消运行中会话                             |
| `/codex continue <session_id> <追加说明>` | 兼容旧式按会话继续                         |

会话卡片展示项目名、会话 ID、上下文 ID、模式、目标状态、执行方式、状态、发起人、开始时间和最终摘要；不展示密钥、绝对路径或完整任务正文。

## 5. 数据与运行时

- 档案保存账号信息、能力、状态和账号池策略；协作机器人密钥只存本地加密凭据库。
- JSON 导入预览在 Rust 内存中最多保留 15 分钟；取消、完成、过期和应用启动清理时均销毁预览与临时验证目录。
- 网关保存请求统计、延迟、健康和冷却状态。
- 协作上下文保存项目全局 scope、稳定 `CODEX_HOME`、active Codex session、memory 开关、goal 状态、model/permissions 快照；群/频道/私聊通过 `collaboration_chat_state` 指向当前 context。
- 每次 Codex turn 的附件和 `last-message.txt` 放入 per-run 目录，同一 context 同时只允许一个 turn 运行。
- 当前档案切换可更新默认 Codex 运行目录、认证数据和桌面端运行状态。
- 应用支持管理本地 Codex 运行目录、任务进程和 OAuth 回调。

## 6. 质量属性与验收

### 6.1 功能验收

1. 手动、多格式 JSON、OAuth 和 API Key 导入可创建独立档案；JSON 导入的无效项不会泄露凭据，且不影响同批已成功项。
2. 已保存 OAuth 档案可直接成为当前档案；确认关闭并切换后复用原 Codex/ChatGPT 客户端数据目录启动相关应用，不出现通用 ChatGPT 登录流程，并保留本机聊天记录、记忆、设置与状态。
3. 网关完成 `/v1/models`、`/v1/responses` 与 `/v1/chat/completions` 调用。
4. 多成员池按优先级和权重路由；成员失败时其他成员继续服务。
5. 状态统计反映本产品网关处理的请求。
6. 飞书、QQ、企业微信、Discord、Telegram 协作机器人可完成配置、项目绑定、会话绑定和项目共享上下文会话创建。

### 6.2 实施验证门槛

- 前端变更：`pnpm test`、`pnpm lint`、`pnpm typecheck`。
- Rust 变更：`pnpm rust:format`、`pnpm rust:lint`、`pnpm rust:test`。
- Tauri capability、CSP、插件或打包变更：`pnpm tauri:build`。
- 交付前完整验证：`pnpm check` 与 `git diff --check`。

## 7. 后续阶段与待确认事项

### 7.1 后续阶段

- 跟进 Gemini 官方新一代交互接口，评估是否从当前 v1beta generateContent 体系迁移。
- 扩展更多通讯软件协作连接器，并深化现有平台的富卡片/权限能力。
- 评估更细粒度的 API Key 用量、模型能力和用量展示。

### 7.2 待确认

1. 首批 OAuth 提供方、授权范围、令牌刷新机制与各自文档。
2. 首批第三方中转站名单、模型映射和 API 兼容性测试矩阵。
3. 局域网部署的目标操作系统、地址策略及管理员责任边界。
