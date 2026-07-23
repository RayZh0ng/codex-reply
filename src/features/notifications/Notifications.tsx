import {
  BellRinging,
  PaperPlaneTilt,
  Plus,
  ShieldCheck,
  Trash,
} from "@phosphor-icons/react";
import { FormEvent, useState } from "react";

import type { ChannelKind, MaskedChannel } from "../../shared/ipc";

interface NotificationsProps {
  channels: MaskedChannel[];
  busy: boolean;
  onSave: (input: Record<string, unknown>) => Promise<void>;
  onTest: (id: string) => Promise<void>;
  onDelete: (id: string, name: string) => void;
}

export function Notifications({
  channels,
  busy,
  onSave,
  onTest,
  onDelete,
}: NotificationsProps) {
  const [showForm, setShowForm] = useState(false);
  return (
    <div className="page notifications-page">
      <header className="page-heading" data-animate="heading">
        <div>
          <p className="section-kicker">Notifications</p>
          <h1>只发送必要的状态</h1>
          <p className="page-subtitle">
            飞书、企业微信和自定义 HTTPS Webhook 均为单向、脱敏的出站通知。
          </p>
        </div>
        <button
          className="primary-button"
          type="button"
          onClick={() => setShowForm(true)}
        >
          <Plus size={18} weight="bold" /> 添加频道
        </button>
      </header>
      <section className="privacy-banner" data-animate="notice">
        <ShieldCheck size={23} weight="fill" />
        <div>
          <strong>隐私保护默认开启</strong>
          <p>通知不会包含 API Key、Webhook URL、任务正文、模型输出或绝对路径。</p>
        </div>
      </section>
      {showForm && (
        <ChannelForm
          busy={busy}
          onCancel={() => setShowForm(false)}
          onSubmit={async (input) => {
            await onSave(input);
            setShowForm(false);
          }}
        />
      )}
      <section className="channel-list" data-animate="cards">
        {channels.length ? (
          channels.map((channel) => (
            <article className="channel-card" key={channel.id}>
              <div className="channel-icon">
                <BellRinging size={23} weight="duotone" />
              </div>
              <div className="channel-main">
                <div>
                  <h2>{channel.name}</h2>
                  <span
                    className={`status-pill compact ${channel.enabled ? "success" : "neutral"}`}
                  >
                    <i />
                    {channel.enabled ? "已启用" : "已停用"}
                  </span>
                </div>
                <p>
                  {kindLabel(channel.kind)} · {channel.endpoint_mask}
                </p>
                <small>最近投递：{statusLabel(channel.last_status)}</small>
              </div>
              <div className="channel-actions">
                <button
                  className="quiet-button"
                  disabled={busy}
                  type="button"
                  onClick={() => void onTest(channel.id)}
                >
                  <PaperPlaneTilt size={17} /> 测试投递
                </button>
                <button
                  className="icon-button danger"
                  aria-label={`删除频道：${channel.name}`}
                  disabled={busy}
                  type="button"
                  onClick={() => onDelete(channel.id, channel.name)}
                >
                  <Trash size={18} />
                </button>
              </div>
            </article>
          ))
        ) : (
          <article className="empty-state">
            <BellRinging size={38} weight="duotone" />
            <h2>还没有通知频道</h2>
            <p>配置一个频道后，可安全接收网关状态和测试投递结果。</p>
            <button
              className="primary-button"
              type="button"
              onClick={() => setShowForm(true)}
            >
              <Plus size={17} /> 添加频道
            </button>
          </article>
        )}
      </section>
    </div>
  );
}

function ChannelForm({
  busy,
  onCancel,
  onSubmit,
}: {
  busy: boolean;
  onCancel: () => void;
  onSubmit: (input: Record<string, unknown>) => Promise<void>;
}) {
  const [kind, setKind] = useState<ChannelKind>("feishu");
  const [name, setName] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [secret, setSecret] = useState("");
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    await onSubmit({
      name,
      kind,
      endpoint,
      signing_secret: secret || null,
      enabled: true,
      confirmed: true,
    });
  };
  return (
    <section className="form-sheet" aria-labelledby="channel-form-title">
      <div className="form-sheet-heading">
        <div>
          <p className="section-kicker">New channel</p>
          <h2 id="channel-form-title">添加通知频道</h2>
        </div>
        <button className="text-button" type="button" onClick={onCancel}>
          取消
        </button>
      </div>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset>
          <legend>通知服务</legend>
          <div className="segmented three">
            <button
              className={kind === "feishu" ? "selected" : ""}
              type="button"
              onClick={() => setKind("feishu")}
            >
              飞书
            </button>
            <button
              className={kind === "wecom" ? "selected" : ""}
              type="button"
              onClick={() => setKind("wecom")}
            >
              企业微信
            </button>
            <button
              className={kind === "custom" ? "selected" : ""}
              type="button"
              onClick={() => setKind("custom")}
            >
              自定义 HTTPS
            </button>
          </div>
        </fieldset>
        <label>
          频道名称
          <input
            required
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="例如：值班通知"
          />
        </label>
        <label>
          Webhook URL
          <input
            required
            type="url"
            autoComplete="off"
            value={endpoint}
            onChange={(event) => setEndpoint(event.target.value)}
            placeholder="https://…"
          />
        </label>
        <label>
          签名密钥（可选）
          <input
            type="password"
            autoComplete="off"
            value={secret}
            onChange={(event) => setSecret(event.target.value)}
            placeholder="仅进入系统安全存储"
          />
        </label>
        <p className="form-note">
          <ShieldCheck size={16} />{" "}
          保存和测试投递都需要明确确认。投递失败不会影响网关或其他频道。
        </p>
        <div className="form-actions">
          <button className="quiet-button" type="button" onClick={onCancel}>
            取消
          </button>
          <button className="primary-button" disabled={busy} type="submit">
            安全保存
          </button>
        </div>
      </form>
    </section>
  );
}

function kindLabel(kind: ChannelKind) {
  return kind === "feishu"
    ? "飞书机器人"
    : kind === "wecom"
      ? "企业微信机器人"
      : "自定义 HTTPS";
}
function statusLabel(status: string) {
  return status === "delivered"
    ? "成功"
    : status === "failed"
      ? "失败，可重试"
      : "尚未测试";
}
