import { open } from "@tauri-apps/plugin-dialog";
import { ChatCircleDots } from "@phosphor-icons/react/ChatCircleDots";
import { CheckCircle } from "@phosphor-icons/react/CheckCircle";
import { Copy } from "@phosphor-icons/react/Copy";
import { DiscordLogo } from "@phosphor-icons/react/DiscordLogo";
import { FolderOpen } from "@phosphor-icons/react/FolderOpen";
import { LinkSimple } from "@phosphor-icons/react/LinkSimple";
import { PaperPlaneTilt } from "@phosphor-icons/react/PaperPlaneTilt";
import { Question } from "@phosphor-icons/react/Question";
import { Robot } from "@phosphor-icons/react/Robot";
import { TelegramLogo } from "@phosphor-icons/react/TelegramLogo";
import { Trash } from "@phosphor-icons/react/Trash";
import { WechatLogo } from "@phosphor-icons/react/WechatLogo";
import { XCircle } from "@phosphor-icons/react/XCircle";
import { FormEvent, type ReactNode, useEffect, useMemo, useState } from "react";

import type {
  CodexSessionSummary,
  CollaborationCallbackStatus,
  CollaborationExecutionTarget,
  CollaborationProjectBinding,
  CollaborationProvider,
  MaskedCollaborationBot,
  MaskedProfile,
} from "../../shared/ipc";
import { Button, Dialog, Select } from "../../shared/ui";

interface ProviderCard {
  id: CollaborationProvider;
  name: string;
  shortName: string;
  description: string;
  icon: ReactNode;
  docs: { label: string; href: string }[];
  commandMode: "text" | "slash";
}

interface CollaborationProps {
  bots: MaskedCollaborationBot[];
  bindings: CollaborationProjectBinding[];
  sessions: CodexSessionSummary[];
  profiles: MaskedProfile[];
  gatewayModelOptions: string[];
  busy: boolean;
  onSaveBot: (input: Record<string, unknown>) => Promise<void>;
  onTestBot: (id: string) => Promise<void>;
  onDeleteBot: (id: string, name: string) => void;
  onSaveBinding: (input: Record<string, unknown>) => Promise<void>;
  onDeleteBinding: (id: string, name: string) => void;
  onRegisterDiscordCommands: (id: string) => Promise<void>;
  onLoadCallbackStatus: () => Promise<CollaborationCallbackStatus>;
  onCancelSession: (id: string) => Promise<void>;
  onContinueSession: (id: string, instruction: string) => Promise<void>;
}

const providers: ProviderCard[] = [
  {
    id: "feishu",
    name: "飞书",
    shortName: "飞书",
    description: "自建应用机器人长连接接收群消息，使用消息与卡片 API 回传任务状态。",
    icon: <Robot size={24} weight="duotone" />,
    commandMode: "text",
    docs: [
      { label: "创建自建应用", href: "https://open.feishu.cn/app" },
      {
        label: "机器人能力",
        href: "https://open.feishu.cn/document/client-docs/bot-v3/add-custom-bot?lang=zh-CN",
      },
      {
        label: "长连接事件",
        href: "https://open.feishu.cn/document/server-docs/event-subscription-guide/event-subscription-configure-/request-url-configuration-case?lang=zh-CN",
      },
    ],
  },
  {
    id: "qq",
    name: "QQ 官方机器人",
    shortName: "QQ",
    description:
      "使用 QQ 官方机器人 WebSocket Gateway 接收群消息，并通过 OpenAPI 发送状态文本。",
    icon: <ChatCircleDots size={24} weight="duotone" />,
    commandMode: "text",
    docs: [
      {
        label: "WebSocket Gateway",
        href: "https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/interface-framework/reference.html",
      },
      {
        label: "API 调用指南",
        href: "https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/api-call-guide.html",
      },
    ],
  },
  {
    id: "wecom",
    name: "企业微信自建应用",
    shortName: "企业微信",
    description:
      "使用自建应用回调 URL 接收消息，完成签名/AES 验证后用应用消息 API 回传。",
    icon: <WechatLogo size={24} weight="duotone" />,
    commandMode: "text",
    docs: [
      {
        label: "企业微信开发文档",
        href: "https://developer.work.weixin.qq.com/document",
      },
    ],
  },
  {
    id: "discord",
    name: "Discord Bot",
    shortName: "Discord",
    description:
      "通过 Discord Gateway 接收 slash command，并可在开启消息内容权限后兼容文本命令。",
    icon: <DiscordLogo size={24} weight="duotone" />,
    commandMode: "slash",
    docs: [
      { label: "Gateway", href: "https://docs.discord.com/developers/events/gateway" },
      {
        label: "Interactions",
        href: "https://docs.discord.com/developers/interactions/receiving-and-responding",
      },
    ],
  },
  {
    id: "telegram",
    name: "Telegram Bot",
    shortName: "Telegram",
    description:
      "使用 Bot API getUpdates 长轮询收取命令，并用 sendMessage / editMessageText 回传状态。",
    icon: <TelegramLogo size={24} weight="duotone" />,
    commandMode: "text",
    docs: [{ label: "Telegram Bot API", href: "https://core.telegram.org/bots/api" }],
  },
];

export function Collaboration({
  bots,
  bindings,
  sessions,
  profiles,
  gatewayModelOptions,
  busy,
  onSaveBot,
  onTestBot,
  onDeleteBot,
  onSaveBinding,
  onDeleteBinding,
  onRegisterDiscordCommands,
  onLoadCallbackStatus,
  onCancelSession,
  onContinueSession,
}: CollaborationProps) {
  const [selectedProvider, setSelectedProvider] =
    useState<CollaborationProvider>("feishu");
  const [helpOpen, setHelpOpen] = useState(false);
  const [callbackStatus, setCallbackStatus] =
    useState<CollaborationCallbackStatus | null>(null);
  const codexProfiles = useMemo(
    () =>
      profiles.filter(
        (profile) =>
          profile.kind === "codex_oauth" &&
          profile.enabled &&
          profile.credential_configured,
      ),
    [profiles],
  );
  const selectedProviderMeta = providerMeta(selectedProvider);
  const selectedBots = bots.filter((bot) => bot.provider === selectedProvider);
  const selectedBindings = bindings.filter(
    (binding) => binding.provider === selectedProvider,
  );
  const selectedSessions = sessions.filter(
    (session) => session.provider === selectedProvider,
  );
  const providerConfigured = selectedBots.length > 0;

  useEffect(() => {
    void onLoadCallbackStatus()
      .then(setCallbackStatus)
      .catch(() => setCallbackStatus(null));
  }, [onLoadCallbackStatus]);

  return (
    <div className="page collaboration-page">
      <header className="page-heading" data-animate="heading">
        <div>
          <h1>连接群聊</h1>
          <p className="page-subtitle">从常用通讯软件向本机 Codex 项目下发任务。</p>
        </div>
        <Button
          leadingIcon={<Question size={17} />}
          onClick={() => setHelpOpen(true)}
          size="sm"
          variant="quiet"
        >
          使用指南
        </Button>
      </header>

      <ProviderGrid
        providers={providers}
        bots={bots}
        selected={selectedProvider}
        onSelect={setSelectedProvider}
      />

      <div className="provider-detail-stage" key={selectedProvider}>
        {!providerConfigured && <OnboardingSummary provider={selectedProviderMeta} />}

        <div className="settings-grid collaboration-settings" data-animate="cards">
          <BotForm busy={busy} provider={selectedProvider} onSubmit={onSaveBot} />
          {providerConfigured ? (
            <BindingForm
              busy={busy}
              bots={selectedBots}
              profiles={codexProfiles}
              gatewayModelOptions={gatewayModelOptions}
              onSubmit={onSaveBinding}
            />
          ) : (
            <NextStepCard provider={selectedProviderMeta} />
          )}
        </div>

        {providerConfigured && (
          <section className="collaboration-management" data-animate="lower">
            <BotList
              bots={selectedBots}
              busy={busy}
              onSaveBot={onSaveBot}
              onTest={onTestBot}
              onDelete={onDeleteBot}
              onRegisterDiscordCommands={onRegisterDiscordCommands}
            />
            {selectedBindings.length > 0 && (
              <BindingList
                bindings={selectedBindings}
                busy={busy}
                onDelete={onDeleteBinding}
              />
            )}
          </section>
        )}

        {selectedSessions.length > 0 && (
          <SessionList
            sessions={selectedSessions}
            busy={busy}
            onCancel={onCancelSession}
            onContinue={onContinueSession}
          />
        )}
      </div>

      <Dialog
        description={`${selectedProviderMeta.shortName}的接入步骤与 /codex 命令。`}
        onClose={() => setHelpOpen(false)}
        open={helpOpen}
        size="lg"
        title={`${selectedProviderMeta.shortName}使用指南`}
        footer={
          <Button onClick={() => setHelpOpen(false)} variant="primary">
            完成
          </Button>
        }
      >
        <div className="collaboration-help-content">
          <SetupGuide
            provider={selectedProviderMeta}
            bots={selectedBots}
            bindings={selectedBindings}
            sessions={selectedSessions}
            callbackStatus={callbackStatus}
          />
          <CommandQuickStart
            provider={selectedProviderMeta}
            bindings={selectedBindings}
            sessions={selectedSessions}
          />
        </div>
      </Dialog>
    </div>
  );
}

function ProviderGrid({
  providers,
  bots,
  selected,
  onSelect,
}: {
  providers: ProviderCard[];
  bots: MaskedCollaborationBot[];
  selected: CollaborationProvider;
  onSelect: (provider: CollaborationProvider) => void;
}) {
  return (
    <section
      aria-label="协作平台"
      className="provider-switcher"
      data-animate="cards"
      role="tablist"
    >
      {providers.map((provider) => {
        const providerBots = bots.filter((bot) => bot.provider === provider.id);
        const status = providerStatus(providerBots);
        return (
          <button
            aria-selected={selected === provider.id}
            className={`provider-tab ${selected === provider.id ? "active" : ""}`}
            key={provider.id}
            onClick={() => onSelect(provider.id)}
            role="tab"
            type="button"
          >
            <span className="provider-tab-icon">{provider.icon}</span>
            <span className="provider-tab-copy">
              <strong>{provider.shortName}</strong>
              <small className={status.className}>
                <i /> {status.label}
              </small>
            </span>
          </button>
        );
      })}
    </section>
  );
}

function OnboardingSummary({ provider }: { provider: ProviderCard }) {
  return (
    <section className="onboarding-summary" data-animate="notice">
      <div className="provider-icon">{provider.icon}</div>
      <div>
        <strong>配置{provider.shortName}</strong>
        <p>保存机器人凭据后，绑定本机项目，再从群聊发送 /codex 命令。</p>
      </div>
      <ol aria-label={`${provider.shortName}配置步骤`}>
        <li>
          <span>1</span>保存机器人
        </li>
        <li>
          <span>2</span>绑定项目
        </li>
        <li>
          <span>3</span>群内运行
        </li>
      </ol>
    </section>
  );
}

function NextStepCard({ provider }: { provider: ProviderCard }) {
  return (
    <section className="form-sheet next-step-card">
      <div className="form-sheet-heading">
        <div>
          <h2>下一步：绑定项目</h2>
          <p>保存{provider.shortName}机器人后，即可选择 Codex 档案和本机目录。</p>
        </div>
        <LinkSimple size={24} />
      </div>
      <div className="next-step-preview" aria-hidden="true">
        <span />
        <span />
        <span />
      </div>
    </section>
  );
}

function SetupGuide({
  provider,
  bots,
  bindings,
  sessions,
  callbackStatus,
}: {
  provider: ProviderCard;
  bots: MaskedCollaborationBot[];
  bindings: CollaborationProjectBinding[];
  sessions: CodexSessionSummary[];
  callbackStatus: CollaborationCallbackStatus | null;
}) {
  const hasBot = bots.length > 0;
  const hasBinding = bindings.length > 0;
  const hasChat = bindings.some((binding) => binding.chat_id);
  const hasSession = sessions.length > 0;
  const wecomCallbackReady =
    provider.id !== "wecom" || bots.some((bot) => bot.callback_public_url);
  const steps = setupSteps(provider, {
    hasBot,
    hasBinding,
    hasChat,
    hasSession,
    wecomCallbackReady,
  });

  return (
    <section className="setup-guide panel-card" data-animate="cards">
      <div className="card-heading">
        <div>
          <p className="section-kicker">{provider.shortName} setup</p>
          <h2>从 0 创建并使用{provider.shortName}连接器</h2>
        </div>
      </div>
      {provider.id === "wecom" && callbackStatus && (
        <div className="inline-hint">
          本机回调入口：<code>{callbackStatus.local_url}</code>
          {callbackStatus.running ? " · 本机服务运行中" : " · 本机服务待启动"}
        </div>
      )}
      <ol className="setup-steps">
        {steps.map((step, index) => (
          <li className={step.done ? "done" : ""} key={step.title}>
            <span className="step-index">
              {step.done ? <CheckCircle size={18} weight="fill" /> : index + 1}
            </span>
            <div>
              {step.href ? (
                <a href={step.href} target="_blank" rel="noreferrer">
                  {step.title}
                </a>
              ) : (
                <strong>{step.title}</strong>
              )}
              <p>{step.detail}</p>
            </div>
          </li>
        ))}
      </ol>
      <div className="doc-links">
        {provider.docs.map((doc) => (
          <a key={doc.href} href={doc.href} target="_blank" rel="noreferrer">
            {doc.label}
          </a>
        ))}
      </div>
    </section>
  );
}

function CommandQuickStart({
  provider,
  bindings,
  sessions,
}: {
  provider: ProviderCard;
  bindings: CollaborationProjectBinding[];
  sessions: CodexSessionSummary[];
}) {
  const firstBinding = bindings[0];
  const firstUnbound = bindings.find((binding) => !binding.chat_id);
  const project = firstBinding?.project_slug ?? "<project>";
  const bindCode = firstUnbound?.bind_code ?? firstBinding?.bind_code ?? "<code>";
  const sessionId = sessions[0]?.id ?? "<session_id>";
  const textCommands = [
    { label: "查看帮助", value: "/codex help" },
    { label: "绑定群聊", value: `/codex bind ${bindCode}` },
    { label: "列出项目", value: "/codex projects" },
    { label: "启动任务", value: `/codex run ${project} 修复当前失败的测试` },
    { label: "列出会话", value: `/codex sessions ${project}` },
    { label: "查看状态", value: `/codex status ${sessionId}` },
    { label: "取消会话", value: `/codex cancel ${sessionId}` },
    { label: "继续会话", value: `/codex continue ${sessionId} 根据最新反馈继续修改` },
  ];
  const commands =
    provider.commandMode === "slash"
      ? textCommands.map((command) => ({
          ...command,
          value: `/codex command:${JSON.stringify(command.value.replace("/codex ", ""))}`,
        }))
      : textCommands;
  return (
    <section className="command-guide panel-card" data-animate="cards">
      <div className="card-heading">
        <div>
          <p className="section-kicker">Commands</p>
          <h2>{provider.shortName} 命令速查</h2>
        </div>
      </div>
      <div className="command-grid">
        {commands.map((command) => (
          <CopyableCommand
            key={command.label}
            label={command.label}
            value={command.value}
          />
        ))}
      </div>
    </section>
  );
}

function CopyableCommand({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    await navigator.clipboard?.writeText(value);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1400);
  };
  return (
    <div className="command-card">
      <span>{label}</span>
      <code>{value}</code>
      <button className="text-button" type="button" onClick={() => void copy()}>
        <Copy size={15} /> {copied ? "已复制" : "复制"}
      </button>
    </div>
  );
}

function BotForm({
  provider,
  busy,
  onSubmit,
}: {
  provider: CollaborationProvider;
  busy: boolean;
  onSubmit: (input: Record<string, unknown>) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [values, setValues] = useState<Record<string, string>>({});
  const [systemPrompt, setSystemPrompt] = useState("");
  const meta = providerMeta(provider);
  const fields = providerFields(provider);
  const setField = (key: string, value: string) =>
    setValues((current) => ({ ...current, [key]: value }));
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    const payload: Record<string, unknown> = {
      id: null,
      provider,
      name,
      enabled: true,
      confirmed: true,
      system_prompt: systemPrompt,
    };
    for (const field of fields) payload[field.key] = values[field.key] ?? "";
    await onSubmit(payload);
    setName("");
    setValues({});
    setSystemPrompt("");
  };
  return (
    <section className="form-sheet">
      <div className="form-sheet-heading">
        <div>
          <h2>配置{meta.shortName}机器人</h2>
          <p>{meta.description}</p>
        </div>
        {meta.icon}
      </div>
      <form onSubmit={(event) => void submit(event)}>
        <label>
          机器人名称
          <input
            required
            value={name}
            placeholder={`例如：${meta.shortName} Codex 助手`}
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        {fields.map((field) => (
          <label key={field.key}>
            {field.label}
            <input
              required={field.required}
              type={field.secret ? "password" : "text"}
              autoComplete="off"
              value={values[field.key] ?? ""}
              placeholder={field.placeholder}
              onChange={(event) => setField(field.key, event.target.value)}
            />
          </label>
        ))}
        <label>
          机器人专属提示词（可选）
          <textarea
            aria-label="机器人专属提示词（可选）"
            value={systemPrompt}
            placeholder="例如：你是本项目的代码评审助手，回复请优先给出结论、变更文件和验证命令。"
            onChange={(event) => setSystemPrompt(event.target.value)}
          />
          <p className="form-note">
            这段提示词会自动附加到该机器人发起的新任务和继续任务，不会展示给群成员。
          </p>
        </label>
        <p className="form-note">{providerNote(provider)}</p>
        <div className="form-actions">
          <button className="primary-button" disabled={busy} type="submit">
            保存机器人
          </button>
        </div>
      </form>
    </section>
  );
}

function BindingForm({
  bots,
  profiles,
  gatewayModelOptions,
  busy,
  onSubmit,
}: {
  bots: MaskedCollaborationBot[];
  profiles: MaskedProfile[];
  gatewayModelOptions: string[];
  busy: boolean;
  onSubmit: (input: Record<string, unknown>) => Promise<void>;
}) {
  const [botId, setBotId] = useState("");
  const [profileId, setProfileId] = useState("");
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [directory, setDirectory] = useState("");
  const [limit, setLimit] = useState(2);
  const [executionTarget, setExecutionTarget] =
    useState<CollaborationExecutionTarget>("profile");
  const [modelId, setModelId] = useState("");
  const gatewayModelSelectOptions = useMemo(() => {
    return Array.from(new Set(gatewayModelOptions))
      .sort((left, right) => left.localeCompare(right))
      .map((model) => ({ value: model, label: model }));
  }, [gatewayModelOptions]);
  const projectSlug = slugify(slug || name);
  const slugMissing = Boolean(name.trim()) && !projectSlug;
  useEffect(() => {
    if (executionTarget === "gateway" && profileId) {
      setProfileId("");
    }
  }, [executionTarget, profileId]);
  const selectDirectory = async () => {
    const selected = await open({
      title: "选择协作项目工作目录",
      directory: true,
      multiple: false,
    });
    if (typeof selected === "string") setDirectory(selected);
  };
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (slugMissing) return;
    await onSubmit({
      id: null,
      bot_id: botId,
      project_name: name,
      project_slug: projectSlug,
      working_directory: directory,
      profile_id: executionTarget === "profile" ? profileId : null,
      enabled: true,
      concurrency_limit: limit,
      execution_target: executionTarget,
      model_id: executionTarget === "gateway" ? modelId : null,
      confirmed: true,
    });
    setName("");
    setSlug("");
    setDirectory("");
    setModelId("");
  };
  return (
    <section className="form-sheet">
      <div className="form-sheet-heading">
        <div>
          <h2>绑定本机项目</h2>
          <p>选择机器人、执行方式和本机目录。</p>
        </div>
        <LinkSimple size={24} />
      </div>
      <form onSubmit={(event) => void submit(event)}>
        <label>
          机器人
          <Select
            ariaLabel="机器人"
            disabled={!bots.length}
            onValueChange={setBotId}
            options={bots.map((bot) => ({
              value: bot.id,
              label: bot.name,
              description: providerLabel(bot.provider),
              keywords: providerLabel(bot.provider),
            }))}
            placeholder={bots.length ? "选择机器人" : "请先保存机器人"}
            value={botId}
          />
        </label>
        {executionTarget === "profile" && (
          <label>
            Codex 档案
            <Select
              ariaLabel="Codex 档案"
              disabled={!profiles.length}
              onValueChange={setProfileId}
              options={profiles.map((profile) => ({
                value: profile.id,
                label: profile.alias,
              }))}
              placeholder={profiles.length ? "选择档案" : "暂无可用档案"}
              value={profileId}
            />
          </label>
        )}
        <label>
          执行方式
          <Select
            ariaLabel="执行方式"
            onValueChange={(value) => {
              setExecutionTarget(value as CollaborationExecutionTarget);
              setModelId("");
            }}
            options={[
              {
                value: "profile",
                label: "Codex 档案直连",
                description: "沿用所选档案凭据启动 Codex",
              },
              {
                value: "gateway",
                label: "API 服务网关",
                description: "请求发送到 Relay /v1 网关并按账号池路由",
              },
            ]}
            value={executionTarget}
          />
        </label>
        {executionTarget === "gateway" && (
          <label>
            默认模型
            <Select
              ariaLabel="默认模型"
              disabled={!gatewayModelSelectOptions.length}
              onValueChange={setModelId}
              options={gatewayModelSelectOptions}
              placeholder={
                gatewayModelSelectOptions.length
                  ? "选择网关模型"
                  : "请先为网关账号池刷新模型"
              }
              value={modelId}
            />
            {!gatewayModelSelectOptions.length && (
              <p className="form-note error-note">
                先刷新可用模型，并确认至少一个账号已启用、加入网关账号池且状态可用。
              </p>
            )}
          </label>
        )}
        <label>
          项目名称
          <input
            required
            value={name}
            placeholder="例如：Codex Relay"
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        <label>
          群命令项目名
          <input
            value={slug}
            onChange={(event) => setSlug(event.target.value)}
            placeholder={name ? slugify(name) : "例如 relay"}
          />
          {slugMissing && (
            <p className="form-note error-note">
              群命令项目名需要至少包含一个英文字母或数字；中文项目名请手动填写，例如
              relay。
            </p>
          )}
        </label>
        <label>
          单项目并发上限
          <input
            type="number"
            min={1}
            max={8}
            value={limit}
            onChange={(event) => setLimit(Number(event.target.value))}
          />
        </label>
        <div className="managed-task-directory">
          <button
            className="quiet-button"
            type="button"
            onClick={() => void selectDirectory()}
          >
            <FolderOpen size={17} /> 选择目录
          </button>
          {directory ? <span>已选择工作目录</span> : <span>尚未选择工作目录</span>}
        </div>
        <div className="form-actions">
          <button
            className="primary-button"
            disabled={
              busy ||
              !botId ||
              (executionTarget === "profile" && !profileId) ||
              !name ||
              slugMissing ||
              !directory ||
              (executionTarget === "gateway" && !modelId)
            }
            type="submit"
          >
            创建绑定
          </button>
        </div>
      </form>
    </section>
  );
}

function BotList({
  bots,
  busy,
  onSaveBot,
  onTest,
  onDelete,
  onRegisterDiscordCommands,
}: {
  bots: MaskedCollaborationBot[];
  busy: boolean;
  onSaveBot: (input: Record<string, unknown>) => Promise<void>;
  onTest: (id: string) => Promise<void>;
  onDelete: (id: string, name: string) => void;
  onRegisterDiscordCommands: (id: string) => Promise<void>;
}) {
  const [editingPromptId, setEditingPromptId] = useState<string | null>(null);
  const [promptDrafts, setPromptDrafts] = useState<Record<string, string>>({});
  const editPrompt = (bot: MaskedCollaborationBot) => {
    setEditingPromptId(bot.id);
    setPromptDrafts((current) => ({
      ...current,
      [bot.id]: current[bot.id] ?? bot.system_prompt ?? "",
    }));
  };
  const savePrompt = async (bot: MaskedCollaborationBot) => {
    await onSaveBot({
      id: bot.id,
      provider: bot.provider,
      name: bot.name,
      enabled: bot.enabled,
      confirmed: true,
      system_prompt: promptDrafts[bot.id] ?? "",
    });
    setEditingPromptId(null);
  };
  return (
    <section className="flat-panel">
      <div className="card-heading">
        <div>
          <h2>机器人连接</h2>
        </div>
      </div>
      <div className="channel-list compact-list">
        {bots.length ? (
          bots.map((bot) => (
            <article className="channel-card" key={bot.id}>
              <div className="channel-icon">{providerMeta(bot.provider).icon}</div>
              <div className="channel-main">
                <div>
                  <h2>{bot.name}</h2>
                  <span
                    className={`status-pill compact ${bot.enabled ? statusClass(bot.connection_status) : "neutral"}`}
                  >
                    <i /> {bot.enabled ? statusLabel(bot.connection_status) : "已停用"}
                  </span>
                </div>
                <p>
                  {providerLabel(bot.provider)} · {bot.credential_mask} ·{" "}
                  {bot.config_summary}
                </p>
                {bot.provider === "wecom" && (
                  <small>
                    {bot.callback_public_url
                      ? "公网回调 URL 已配置"
                      : "等待配置公网回调 URL"}
                  </small>
                )}
                {bot.last_error && <small>{bot.last_error}</small>}
                {bot.system_prompt && (
                  <small className="prompt-state">已配置专属提示词</small>
                )}
                {editingPromptId === bot.id && (
                  <div className="prompt-editor">
                    <label>
                      机器人专属提示词
                      <textarea
                        value={promptDrafts[bot.id] ?? ""}
                        placeholder="输入该机器人发起任务时自动附加的提示词"
                        onChange={(event) =>
                          setPromptDrafts((current) => ({
                            ...current,
                            [bot.id]: event.target.value,
                          }))
                        }
                      />
                    </label>
                    <div className="prompt-editor-actions">
                      <button
                        className="quiet-button"
                        disabled={busy}
                        type="button"
                        onClick={() => void savePrompt(bot)}
                      >
                        保存提示词
                      </button>
                      <button
                        className="text-button"
                        disabled={busy}
                        type="button"
                        onClick={() => setEditingPromptId(null)}
                      >
                        取消编辑
                      </button>
                    </div>
                  </div>
                )}
              </div>
              <div className="channel-actions">
                <button
                  className="quiet-button"
                  disabled={busy}
                  type="button"
                  onClick={() => editPrompt(bot)}
                >
                  编辑提示词
                </button>
                {bot.provider === "discord" && (
                  <button
                    className="quiet-button"
                    disabled={busy}
                    type="button"
                    onClick={() => void onRegisterDiscordCommands(bot.id)}
                  >
                    注册命令
                  </button>
                )}
                <button
                  className="quiet-button"
                  disabled={busy}
                  type="button"
                  onClick={() => void onTest(bot.id)}
                >
                  <PaperPlaneTilt size={17} /> 测试
                </button>
                <button
                  className="icon-button danger"
                  disabled={busy}
                  type="button"
                  aria-label={`删除机器人：${bot.name}`}
                  onClick={() => onDelete(bot.id, bot.name)}
                >
                  <Trash size={18} />
                </button>
              </div>
            </article>
          ))
        ) : (
          <p className="muted-copy">
            还没有协作机器人，请先选择平台并按从 0 引导保存配置。
          </p>
        )}
      </div>
    </section>
  );
}

function BindingList({
  bindings,
  busy,
  onDelete,
}: {
  bindings: CollaborationProjectBinding[];
  busy: boolean;
  onDelete: (id: string, name: string) => void;
}) {
  return (
    <section className="flat-panel">
      <div className="card-heading">
        <div>
          <h2>群项目绑定</h2>
        </div>
      </div>
      <div className="channel-list compact-list">
        {bindings.length ? (
          bindings.map((binding) => (
            <article className="channel-card" key={binding.id}>
              <div className="channel-icon">
                <ChatCircleDots size={23} weight="duotone" />
              </div>
              <div className="channel-main">
                <div>
                  <h2>{binding.project_name}</h2>
                  <span
                    className={`status-pill compact ${binding.chat_id ? "success" : "neutral"}`}
                  >
                    <i /> {binding.chat_id ? "已绑定群" : "待群内绑定"}
                  </span>
                </div>
                <p>
                  {providerLabel(binding.provider)} · /codex run {binding.project_slug}{" "}
                  …
                </p>
                <dl
                  className="binding-facts"
                  aria-label={`${binding.project_name} 执行设置`}
                >
                  <div>
                    <dt>执行方式</dt>
                    <dd>{bindingExecutionLabel(binding)}</dd>
                  </div>
                  <div>
                    <dt>模型 / 档案</dt>
                    <dd>
                      {binding.execution_target === "gateway"
                        ? (binding.model_id ?? "默认网关模型")
                        : profileAliasLabel(binding.profile_alias)}
                    </dd>
                  </div>
                  <div>
                    <dt>绑定码</dt>
                    <dd>{binding.bind_code}</dd>
                  </div>
                  <div>
                    <dt>群状态</dt>
                    <dd>{binding.chat_id ? "已绑定" : "待绑定"}</dd>
                  </div>
                </dl>
              </div>
              <div className="channel-actions">
                <button
                  className="icon-button danger"
                  disabled={busy}
                  type="button"
                  aria-label={`删除项目绑定：${binding.project_name}`}
                  onClick={() => onDelete(binding.id, binding.project_name)}
                >
                  <Trash size={18} />
                </button>
              </div>
            </article>
          ))
        ) : (
          <p className="muted-copy">
            创建绑定后，在目标群或频道发送 /codex bind &lt;code&gt; 完成群绑定。
          </p>
        )}
      </div>
    </section>
  );
}

function SessionList({
  sessions,
  busy,
  onCancel,
  onContinue,
}: {
  sessions: CodexSessionSummary[];
  busy: boolean;
  onCancel: (id: string) => Promise<void>;
  onContinue: (id: string, instruction: string) => Promise<void>;
}) {
  const [continueText, setContinueText] = useState<Record<string, string>>({});
  return (
    <section className="flat-panel sessions-panel" data-animate="cards">
      <div className="card-heading">
        <div>
          <h2>Codex 会话</h2>
        </div>
      </div>
      <div className="channel-list">
        {sessions.length ? (
          sessions.map((session) => (
            <article className="channel-card" key={session.id}>
              <div className="channel-icon">
                <Robot size={23} weight="duotone" />
              </div>
              <div className="channel-main">
                <div>
                  <h2>{session.project_name}</h2>
                  <span
                    className={`status-pill compact ${sessionStatusClass(session.relay_status)}`}
                  >
                    <i /> {sessionStatusLabel(session.relay_status)}
                  </span>
                </div>
                <p>
                  {providerLabel(session.provider)} · {shortId(session.id)} ·{" "}
                  {profileAliasLabel(session.profile_alias)} ·{" "}
                  {sessionExecutionLabel(session)}
                </p>
                <small>{session.summary ?? "暂无摘要"}</small>
                {session.relay_status === "failed" && session.last_error && (
                  <small className="session-error">
                    失败原因：{session.last_error}
                  </small>
                )}
                {session.codex_session_id && (
                  <small>Codex session：{shortId(session.codex_session_id)}</small>
                )}
                <div className="inline-form">
                  <input
                    value={continueText[session.id] ?? ""}
                    onChange={(event) =>
                      setContinueText((current) => ({
                        ...current,
                        [session.id]: event.target.value,
                      }))
                    }
                    placeholder="追加说明后继续会话"
                  />
                  <button
                    className="quiet-button"
                    disabled={busy || !(continueText[session.id] ?? "").trim()}
                    type="button"
                    onClick={() =>
                      void onContinue(
                        session.id,
                        (continueText[session.id] ?? "").trim(),
                      )
                    }
                  >
                    继续
                  </button>
                </div>
              </div>
              <div className="channel-actions">
                {session.relay_status === "running" && (
                  <button
                    className="quiet-button danger"
                    disabled={busy}
                    type="button"
                    onClick={() => void onCancel(session.id)}
                  >
                    <XCircle size={17} /> 取消
                  </button>
                )}
              </div>
            </article>
          ))
        ) : (
          <p className="muted-copy">还没有从协作平台启动的 Codex 会话。</p>
        )}
      </div>
    </section>
  );
}

function setupSteps(
  provider: ProviderCard,
  progress: {
    hasBot: boolean;
    hasBinding: boolean;
    hasChat: boolean;
    hasSession: boolean;
    wecomCallbackReady: boolean;
  },
) {
  const platformSteps: Record<
    CollaborationProvider,
    { title: string; detail: string; href?: string; done: boolean }[]
  > = {
    feishu: [
      {
        title: "创建企业自建应用",
        detail: "打开飞书开放平台创建企业自建应用并进入应用管理后台。",
        href: "https://open.feishu.cn/app",
        done: progress.hasBot,
      },
      {
        title: "开启机器人与长连接事件",
        detail: "启用机器人能力，订阅接收消息事件和卡片交互回调。",
        href: "https://open.feishu.cn/document/server-docs/event-subscription-guide/event-subscription-configure-/request-url-configuration-case?lang=zh-CN",
        done: progress.hasBot,
      },
    ],
    qq: [
      {
        title: "创建 QQ 官方机器人",
        detail: "在 QQ 机器人平台创建应用，记录 App ID 和 Client Secret。",
        href: "https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/api-call-guide.html",
        done: progress.hasBot,
      },
      {
        title: "开启 Gateway 消息事件",
        detail: "添加机器人到群并允许接收群消息或 @ 消息事件。",
        href: "https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/interface-framework/reference.html",
        done: progress.hasBot,
      },
    ],
    wecom: [
      {
        title: "创建企业微信自建应用",
        detail: "在企业微信管理后台创建自建应用，记录 Corp ID、Agent ID 和 Secret。",
        href: "https://developer.work.weixin.qq.com/document",
        done: progress.hasBot,
      },
      {
        title: "配置回调 URL",
        detail:
          "把公网 HTTPS / 内网穿透 URL 指向 Relay 本机回调入口，并填写 Token 与 EncodingAESKey。",
        done: progress.wecomCallbackReady,
      },
    ],
    discord: [
      {
        title: "创建 Discord Application 与 Bot",
        detail:
          "在 Developer Portal 创建应用、添加 Bot，并记录 Application ID 和 Bot Token。",
        href: "https://docs.discord.com/developers/docs/intro",
        done: progress.hasBot,
      },
      {
        title: "注册 slash command",
        detail:
          "保存机器人后点击“注册命令”，把 /codex command 安装到目标 Guild 或全局。",
        href: "https://docs.discord.com/developers/interactions/receiving-and-responding",
        done: progress.hasBot,
      },
    ],
    telegram: [
      {
        title: "通过 BotFather 创建 Bot",
        detail: "在 Telegram 中创建 Bot，复制 Bot Token，并把 Bot 加入目标群或频道。",
        href: "https://core.telegram.org/bots/features#botfather",
        done: progress.hasBot,
      },
      {
        title: "启用 getUpdates 长轮询",
        detail: "保存 Token 后 Relay 会在本机后台轮询更新，并自动记录 update offset。",
        href: "https://core.telegram.org/bots/api#getupdates",
        done: progress.hasBot,
      },
    ],
  };
  return [
    ...platformSteps[provider.id],
    {
      title: "保存机器人配置",
      detail: "回到 Codex Relay 填写平台凭据。敏感字段只进入本机加密凭据库。",
      done: progress.hasBot,
    },
    {
      title: "创建项目绑定",
      detail: "选择本机项目目录和 Codex OAuth 档案，生成一次性绑定码。",
      done: progress.hasBinding,
    },
    {
      title: "群内绑定 chat_id",
      detail: "在目标群、频道或私聊发送 /codex bind <code>，Relay 会记录该对话 ID。",
      done: progress.hasChat,
    },
    {
      title: "开始群聊任务",
      detail:
        "发送 /codex help 或 /codex run <project> <任务说明> 创建会话。Discord 可使用 /codex command。",
      done: progress.hasSession,
    },
  ];
}

function bindingExecutionLabel(binding: CollaborationProjectBinding) {
  return binding.execution_target === "gateway"
    ? `API 网关${binding.model_id ? ` · ${binding.model_id}` : ""}`
    : "档案直连";
}

function sessionExecutionLabel(session: CodexSessionSummary) {
  return session.execution_target === "gateway"
    ? `API 网关${session.model_id ? ` · ${session.model_id}` : ""}`
    : "档案直连";
}

function profileAliasLabel(alias: string | null) {
  return alias?.trim() || "网关全局设置";
}

function providerFields(provider: CollaborationProvider) {
  return {
    feishu: [
      { key: "app_id", label: "App ID", placeholder: "cli_xxx", required: true },
      {
        key: "app_secret",
        label: "App Secret",
        placeholder: "",
        required: true,
        secret: true,
      },
    ],
    qq: [
      { key: "app_id", label: "App ID", placeholder: "机器人 App ID", required: true },
      {
        key: "client_secret",
        label: "Client Secret",
        placeholder: "",
        required: true,
        secret: true,
      },
    ],
    wecom: [
      { key: "corp_id", label: "Corp ID", placeholder: "ww...", required: true },
      { key: "agent_id", label: "Agent ID", placeholder: "1000002", required: true },
      {
        key: "secret",
        label: "应用 Secret",
        placeholder: "",
        required: true,
        secret: true,
      },
      {
        key: "token",
        label: "回调 Token",
        placeholder: "",
        required: true,
        secret: true,
      },
      {
        key: "encoding_aes_key",
        label: "EncodingAESKey",
        placeholder: "43 位 EncodingAESKey",
        required: true,
        secret: true,
      },
      {
        key: "callback_public_url",
        label: "公网回调 URL",
        placeholder: "https://example.com/collaboration/wecom/<bot_id>",
        required: false,
      },
    ],
    discord: [
      {
        key: "application_id",
        label: "Application ID",
        placeholder: "Discord Application ID",
        required: true,
      },
      {
        key: "bot_token",
        label: "Bot Token",
        placeholder: "",
        required: true,
        secret: true,
      },
      {
        key: "guild_id",
        label: "Guild ID（可选）",
        placeholder: "留空则注册全局命令",
        required: false,
      },
    ],
    telegram: [
      {
        key: "bot_token",
        label: "Bot Token",
        placeholder: "123456:ABC...",
        required: true,
        secret: true,
      },
    ],
  }[provider];
}

function providerNote(provider: CollaborationProvider) {
  return {
    feishu: "保存后后台会启动飞书长连接，接收消息事件和卡片交互回调。",
    qq: "保存后后台会连接 QQ 官方 Gateway，并通过 OpenAPI 发送任务状态。",
    wecom: "企业微信需要公网 HTTPS / 反向代理 / 内网穿透 URL 指向本机回调入口。",
    discord: "保存后后台会连接 Discord Gateway；推荐注册 slash command 后使用。",
    telegram: "保存后后台会通过 Telegram getUpdates 长轮询接收 /codex 命令。",
  }[provider];
}

function providerStatus(bots: MaskedCollaborationBot[]) {
  if (!bots.length) return { label: "未配置", className: "neutral" };
  if (bots.some((bot) => bot.connection_status === "connected")) {
    return { label: "已连接", className: "success" };
  }
  if (bots.some((bot) => bot.connection_status === "connecting")) {
    return { label: "连接中", className: "success" };
  }
  if (bots.some((bot) => bot.connection_status === "callback_required")) {
    return { label: "回调待配置", className: "neutral" };
  }
  if (bots.some((bot) => bot.connection_status === "failed")) {
    return { label: "失败", className: "danger" };
  }
  return { label: "可配置", className: "success" };
}

function providerMeta(provider: CollaborationProvider) {
  return providers.find((item) => item.id === provider) ?? providers[0];
}

function providerLabel(provider: CollaborationProvider) {
  return providerMeta(provider).shortName;
}

function statusLabel(status: string) {
  return status === "connected"
    ? "已连接"
    : status === "connecting"
      ? "连接中"
      : status === "callback_required"
        ? "回调待配置"
        : status === "failed"
          ? "连接失败"
          : status === "disabled"
            ? "已停用"
            : "已配置";
}

function statusClass(status: string) {
  return status === "connected" || status === "connecting" || status === "configured"
    ? "success"
    : status === "failed"
      ? "danger"
      : "neutral";
}

function sessionStatusClass(status: string) {
  return status === "running" ? "success" : status === "failed" ? "danger" : "neutral";
}

function sessionStatusLabel(status: string) {
  return status === "running"
    ? "运行中"
    : status === "completed"
      ? "已完成"
      : status === "failed"
        ? "失败"
        : status === "cancelled"
          ? "已取消"
          : status;
}

function slugify(value: string) {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function shortId(value: string) {
  return value.slice(0, 8);
}
