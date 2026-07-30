import { Key } from "@phosphor-icons/react/Key";
import { LockKey } from "@phosphor-icons/react/LockKey";
import { Play } from "@phosphor-icons/react/Play";
import { Plus } from "@phosphor-icons/react/Plus";
import { Power } from "@phosphor-icons/react/Power";
import { ShieldCheck } from "@phosphor-icons/react/ShieldCheck";
import { Trash } from "@phosphor-icons/react/Trash";
import { FormEvent, useCallback, useEffect, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";

import {
  api,
  type CreatedClientKey,
  type GatewayCodexConfigStatus,
  type GatewayStatus,
  type MaskedClientKey,
} from "../../shared/ipc";
import { Select } from "../../shared/ui/Select";

const NO_OAUTH_PROFILE = "__none__";

interface GatewayProps {
  gateway: GatewayStatus;
  busy: boolean;
  onSave: (input: Record<string, unknown>) => Promise<void>;
  onStart: () => Promise<void>;
  onStop: () => Promise<void>;
  onNotice: (message: string) => void;
  onNavigateProfiles: () => void;
  onRefresh: () => Promise<void>;
}

export function Gateway({
  gateway,
  busy,
  onSave,
  onStart,
  onStop,
  onNotice,
  onNavigateProfiles,
  onRefresh,
}: GatewayProps) {
  const [keys, setKeys] = useState<MaskedClientKey[]>([]);
  const [newKey, setNewKey] = useState<CreatedClientKey | null>(null);
  const [keyName, setKeyName] = useState("");
  const [loadingKeys, setLoadingKeys] = useState(true);
  const [codexConfig, setCodexConfig] = useState<GatewayCodexConfigStatus | null>(null);
  const oauthCandidates = codexConfig?.oauth_profile_options ?? [];
  const hasAvailableOAuthCandidate = oauthCandidates.some(
    (profile) => profile.available,
  );
  const selectedOAuthOption = codexConfig?.oauth_profile_id
    ? oauthCandidates.find((option) => option.id === codexConfig.oauth_profile_id)
    : undefined;
  const selectedOAuthMissing =
    codexConfig?.oauth_profile_id && !selectedOAuthOption ? codexConfig : null;
  const oauthProfileOptions = [
    {
      value: NO_OAUTH_PROFILE,
      label: "不绑定登录档案",
      description: "只切换模型请求，不改写 Codex 登录态",
    },
    ...oauthCandidates.map((profile) => ({
      value: profile.id,
      label: profile.alias,
      disabled: !profile.available,
      description: profile.available
        ? "OAuth 授权登录态解锁"
        : (profile.reason ?? "需要重新检查登录状态"),
    })),
    ...(selectedOAuthMissing
      ? [
          {
            value: selectedOAuthMissing.oauth_profile_id ?? "",
            label:
              selectedOAuthMissing.oauth_profile_alias ??
              selectedOAuthMissing.oauth_profile_id ??
              "已绑定档案",
            disabled: true,
            description: "已绑定档案当前不可读，请重新授权或清空绑定",
          },
        ]
      : []),
  ];
  const reloadKeys = useCallback(async () => {
    setLoadingKeys(true);
    try {
      setKeys(await api.listClientKeys());
    } catch {
      setKeys([]);
    } finally {
      setLoadingKeys(false);
    }
  }, []);
  const reloadCodexConfig = useCallback(async () => {
    try {
      setCodexConfig(await api.codexGatewayConfigStatus());
    } catch {
      setCodexConfig(null);
    }
  }, []);
  const reloadGatewayState = useCallback(async () => {
    await Promise.all([reloadKeys(), reloadCodexConfig(), onRefresh()]);
  }, [onRefresh, reloadCodexConfig, reloadKeys]);
  useEffect(() => {
    void reloadKeys();
  }, [reloadKeys]);
  useEffect(() => {
    void reloadCodexConfig();
  }, [reloadCodexConfig]);
  const createKey = async () => {
    const created = await api.createClientKey(keyName);
    setNewKey(created);
    setKeyName("");
    await reloadGatewayState();
  };
  const revealKey = async (key: MaskedClientKey) => {
    try {
      const plaintext = await api.revealClientKey(key.id);
      setNewKey({ key, plaintext_once: plaintext });
      onNotice("客户端 Key 已显示，请只复制到受信任客户端。");
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "客户端 Key 无法显示。");
    }
  };
  const rotateKey = async (id: string) => {
    try {
      const rotated = await api.rotateClientKey(id);
      setNewKey(rotated);
      await reloadGatewayState();
      onNotice("客户端 Key 已轮换，旧 Key 立即失效。");
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "客户端 Key 轮换未完成。");
    }
  };
  const revoke = async (id: string) => {
    await api.revokeClientKey(id);
    await reloadGatewayState();
    onNotice("客户端 Key 已撤销，受影响客户端需要使用新 Key。");
  };
  const toggleCodexGateway = async () => {
    try {
      const shouldDisable = codexConfig?.enabled && !codexConfig.needs_repair;
      const next = shouldDisable
        ? await api.disableCodexGateway()
        : await api.enableCodexGateway();
      setCodexConfig(next);
      await Promise.all([reloadKeys(), onRefresh()]);
      onNotice(next.message);
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "Codex 网关切换未完成。");
    }
  };
  const setCodexOAuthProfile = async (value: string) => {
    try {
      const profileId = value === NO_OAUTH_PROFILE ? null : value;
      const next = await api.setCodexGatewayOAuthProfile(profileId);
      setCodexConfig(next);
      onNotice(
        profileId
          ? "Codex 网关 OAuth 登录档案已绑定。"
          : "Codex 网关 OAuth 登录档案已清空。",
      );
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "OAuth 登录档案未保存。");
    }
  };
  const codexActionRequiresRunning = !codexConfig?.enabled || codexConfig.needs_repair;
  const codexButtonLabel = codexConfig?.needs_repair
    ? "修复 Codex Key"
    : codexConfig?.enabled
      ? "恢复原 Codex 配置"
      : "设为 Codex 网关";
  const exportCa = async () => {
    try {
      const destination = await save({
        title: "导出 Relay 局域网 CA",
        defaultPath: "codex-relay-gateway-ca.pem",
        filters: [{ name: "PEM certificate", extensions: ["pem"] }],
      });
      if (!destination) return;
      await api.exportGatewayCa(destination);
      onNotice("CA 证书已导出。请在需要访问局域网网关的客户端上信任该证书。");
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "CA 导出未完成。");
    }
  };
  const trustCa = async () => {
    try {
      await api.trustGatewayCa();
      onNotice("Relay CA 已加入当前系统信任存储。请重新启动 Codex 会话后重试。");
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "无法安装 Relay CA。");
    }
  };
  return (
    <div className="page gateway-page">
      <header className="page-heading" data-animate="heading">
        <div>
          <p className="section-kicker">Gateway</p>
          <h1>本机 / 局域网 API 网关</h1>
          <p className="page-subtitle">
            所有入口均使用 HTTPS。可限制为
            127.0.0.1，也可绑定检测到的私有局域网地址；始终需要客户端 Key。
          </p>
          <div className="gateway-heading-meta">
            <code>{gateway.service_url}</code>
            <span>{gateway.available_profiles} 个可用 API 成员</span>
          </div>
        </div>
        <button
          className={gateway.running ? "danger-button" : "primary-button"}
          disabled={busy}
          type="button"
          onClick={() => void (gateway.running ? onStop() : onStart())}
        >
          {gateway.running ? (
            <Power size={18} weight="bold" />
          ) : (
            <Play size={18} weight="fill" />
          )}
          {gateway.running ? "停止服务" : "启动 API 服务"}
        </button>
      </header>
      <GatewayStatusSummary gateway={gateway} />
      {gateway.available_profiles === 0 && (
        <section className="gateway-warning" aria-label="网关账号成员提示">
          <div className="gateway-warning-row">
            <div>
              <strong>当前没有网关账号成员</strong>
              <p>
                {gateway.running
                  ? "服务已启动，但还没有可用账号成员。请到档案页刷新模型并把账号加入网关。"
                  : "启动前请先到档案页刷新模型并把账号加入网关；也可先完成监听与证书配置。"}
              </p>
            </div>
            <button
              className="primary-button compact-action"
              type="button"
              onClick={onNavigateProfiles}
            >
              去档案加入网关
            </button>
          </div>
        </section>
      )}
      {newKey && (
        <SecretOnce
          value={newKey.plaintext_once}
          name={newKey.key.name}
          onClose={() => setNewKey(null)}
        />
      )}
      <section className="gateway-layout" data-animate="gateway">
        <GatewayForm gateway={gateway} busy={busy} onSave={onSave} />
        <aside className="gateway-side">
          <article className="surface-card security-card gateway-card">
            <div className="card-heading">
              <div>
                <p className="section-kicker">Protection</p>
                <h2>访问保护</h2>
              </div>
              <ShieldCheck size={23} weight="duotone" />
            </div>
            <ul className="security-list">
              <li>
                <span>HTTPS 证书</span>
                <strong>
                  {gateway.certificate_ready ? "已就绪" : "首次启动时生成"}
                </strong>
              </li>
              <li>
                <span>监听范围</span>
                <strong>
                  {gateway.bind_mode === "loopback"
                    ? "仅本机 · 127.0.0.1"
                    : `局域网 · ${gateway.bind_address}`}
                </strong>
              </li>
              <li>
                <span>客户端鉴权</span>
                <strong>{gateway.client_key_count} 个有效 Key</strong>
              </li>
            </ul>
            <div className="gateway-inline-actions">
              <button
                className="quiet-button"
                disabled={busy || !gateway.certificate_ready}
                type="button"
                onClick={() => void exportCa()}
              >
                导出 CA
              </button>
              <button
                className="quiet-button"
                disabled={busy || !gateway.certificate_ready}
                type="button"
                onClick={() => void trustCa()}
              >
                信任此 Mac
              </button>
            </div>
          </article>
          <article className="surface-card security-card gateway-card codex-gateway-card">
            <div className="card-heading">
              <div>
                <p className="section-kicker">Codex</p>
                <h2>Codex 网关切换</h2>
              </div>
              <LockKey size={23} weight="duotone" />
            </div>
            <div className="codex-config-overview">
              <span
                className={`codex-config-status ${
                  codexConfig?.enabled && !codexConfig.needs_repair ? "" : "is-disabled"
                }`}
              >
                {codexConfig?.needs_repair
                  ? "需要修复 Codex Key"
                  : codexConfig?.enabled
                    ? "已接入 Relay 网关"
                    : "未接入 Relay 网关"}
              </span>
              <p className="muted-copy codex-config-copy">
                {codexConfig?.message ?? "正在读取 Codex 配置状态…"}
              </p>
            </div>
            <div className="codex-config-control-row">
              <label className="gateway-oauth-profile">
                <span className="gateway-field-label">
                  OAuth 登录档案
                  <span>可选</span>
                </span>
                <Select
                  ariaLabel="OAuth 登录档案"
                  disabled={busy || !codexConfig}
                  onValueChange={(value) => void setCodexOAuthProfile(value)}
                  options={oauthProfileOptions}
                  placeholder="不绑定登录档案"
                  value={codexConfig?.oauth_profile_id ?? NO_OAUTH_PROFILE}
                />
              </label>
              <button
                className={`codex-config-button ${
                  codexConfig?.enabled && !codexConfig.needs_repair
                    ? "quiet-button"
                    : "primary-button"
                }`}
                disabled={
                  busy ||
                  !codexConfig ||
                  (codexActionRequiresRunning && !gateway.running)
                }
                type="button"
                onClick={() => void toggleCodexGateway()}
              >
                {codexButtonLabel}
              </button>
            </div>
            <p className="codex-config-helper">
              {codexConfig?.oauth_profile_id
                ? codexConfig.oauth_profile_available
                  ? `当前登录档案：${codexConfig.oauth_profile_alias ?? codexConfig.oauth_profile_id}`
                  : (selectedOAuthOption?.reason ??
                    "已绑定的 OAuth 登录档案当前不可读，请重新授权或清空绑定。")
                : hasAvailableOAuthCandidate
                  ? "未绑定时保留本机登录档案，只把模型请求路由到账号池成员。"
                  : "还没有可用于登录态解锁的 OAuth 授权档案；JSON 导入账号只用于反代账号池。"}
            </p>
          </article>
          <article className="surface-card key-card gateway-card">
            <div className="card-heading">
              <div>
                <p className="section-kicker">Client access</p>
                <h2>客户端 Key</h2>
              </div>
              <Key size={22} />
            </div>
            <div className="key-create">
              <input
                value={keyName}
                onChange={(event) => setKeyName(event.target.value)}
                placeholder="Key 名称，例如：Alice 的 Mac"
              />
              <button
                className="icon-button accent"
                aria-label="创建客户端 Key"
                disabled={busy || !keyName.trim()}
                type="button"
                onClick={() => void createKey()}
              >
                <Plus size={18} weight="bold" />
              </button>
            </div>
            {loadingKeys ? (
              <p className="muted-copy">正在读取安全元数据…</p>
            ) : keys.length ? (
              <ul className="key-list">
                {keys.map((key) => (
                  <li key={key.id}>
                    <div>
                      <strong>{key.name}</strong>
                      {key.managed_by === "codex_gateway" && (
                        <span className="managed-key-badge">Codex 自动管理</span>
                      )}
                      <span>{key.masked_value}</span>
                    </div>
                    {key.can_revoke && (
                      <div className="key-actions">
                        <button
                          className="quiet-button compact-action"
                          type="button"
                          aria-label={`查看 ${key.name}`}
                          onClick={() => void revealKey(key)}
                        >
                          查看
                        </button>
                        <button
                          className="quiet-button compact-action"
                          type="button"
                          aria-label={`轮换 ${key.name}`}
                          onClick={() => void rotateKey(key.id)}
                        >
                          轮换
                        </button>
                        <button
                          className="icon-button danger"
                          type="button"
                          aria-label={`撤销 ${key.name}`}
                          onClick={() => void revoke(key.id)}
                        >
                          <Trash size={17} />
                        </button>
                      </div>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="muted-copy">创建一个 Key 后，局域网客户端才能调用网关。</p>
            )}
          </article>
        </aside>
      </section>
    </div>
  );
}

function GatewayStatusSummary({ gateway }: { gateway: GatewayStatus }) {
  return (
    <section className="gateway-status-summary" aria-label="网关状态摘要">
      <div className="gateway-status-main">
        <span className={`status-pill ${gateway.running ? "success" : "neutral"}`}>
          <i /> {gateway.running ? "服务运行中" : "服务未启动"}
        </span>
        <code className="gateway-endpoint">{gateway.service_url}</code>
      </div>
      <div className="gateway-status-actions" aria-label="网关关键状态">
        <span className="gateway-status-meta">
          {gateway.available_profiles} 个账号池成员
        </span>
        <span className="gateway-status-meta">{gateway.cooling_profiles} 个冷却中</span>
        <span className="gateway-status-meta">
          {gateway.client_key_count} 个客户端 Key
        </span>
        <span className="gateway-status-meta">
          证书{gateway.certificate_ready ? "就绪" : "待生成"}
        </span>
      </div>
    </section>
  );
}

function GatewayForm({
  gateway,
  busy,
  onSave,
}: {
  gateway: GatewayStatus;
  busy: boolean;
  onSave: (input: Record<string, unknown>) => Promise<void>;
}) {
  const [bindMode, setBindMode] = useState<GatewayStatus["bind_mode"]>(
    gateway.bind_mode,
  );
  const [address, setAddress] = useState(gateway.bind_address);
  const [port, setPort] = useState(String(gateway.port));
  const [cidrs, setCidrs] = useState(gateway.cidrs.join(", "));
  const [proxyMode, setProxyMode] = useState(gateway.upstream_proxy_mode);
  const [proxyUrl, setProxyUrl] = useState("");
  useEffect(() => {
    const selected = gateway.available_addresses.some(
      (candidate) => candidate.address === gateway.bind_address,
    )
      ? gateway.bind_address
      : (gateway.available_addresses.find((candidate) => candidate.is_default)
          ?.address ?? gateway.bind_address);
    setAddress(selected);
    setBindMode(gateway.bind_mode);
    setPort(String(gateway.port));
    setCidrs(gateway.cidrs.join(", "));
    setProxyMode(gateway.upstream_proxy_mode);
    setProxyUrl("");
  }, [gateway]);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    await onSave({
      bind_mode: bindMode,
      bind_address: bindMode === "loopback" ? "127.0.0.1" : address,
      port: Number(port),
      cidrs:
        bindMode === "loopback"
          ? []
          : cidrs
              .split(",")
              .map((item) => item.trim())
              .filter(Boolean),
      confirmed_lan: bindMode === "lan",
      upstream_proxy_mode: proxyMode,
      upstream_proxy_url:
        proxyMode === "manual" && proxyUrl.trim() ? proxyUrl.trim() : null,
    });
  };
  const addressOptions = gateway.available_addresses.map((candidate) => ({
    value: candidate.address,
    label: `${candidate.name} · ${candidate.address}${candidate.is_default ? "（默认）" : ""}`,
  }));
  if (address && !addressOptions.some((option) => option.value === address)) {
    addressOptions.unshift({
      value: address,
      label: `当前地址 · ${address}`,
    });
  }
  return (
    <form
      className="surface-card gateway-form"
      onSubmit={(event) => void submit(event)}
    >
      <div className="card-heading">
        <div>
          <p className="section-kicker">Configuration</p>
          <h2>服务配置</h2>
        </div>
        <LockKey size={23} weight="duotone" />
      </div>
      <label>
        监听模式
        <Select
          ariaLabel="监听模式"
          onValueChange={(value) => setBindMode(value as GatewayStatus["bind_mode"])}
          options={[
            { value: "loopback", label: "仅本机 · 127.0.0.1" },
            { value: "lan", label: "局域网设备" },
          ]}
          value={bindMode}
        />
      </label>
      {bindMode === "lan" && (
        <label>
          检测到的局域网地址
          <Select
            ariaLabel="检测到的局域网地址"
            onValueChange={setAddress}
            options={addressOptions}
            placeholder="等待检测局域网地址"
            value={address}
          />
        </label>
      )}
      <label>
        服务端口
        <input
          value={port}
          onChange={(event) => setPort(event.target.value)}
          inputMode="numeric"
        />
      </label>
      {bindMode === "lan" && (
        <label>
          允许的 CIDR（可选）
          <input
            value={cidrs}
            onChange={(event) => setCidrs(event.target.value)}
            placeholder="留空允许局域网设备；例如：192.168.1.0/24"
          />
        </label>
      )}
      <label>
        上游代理
        <Select
          ariaLabel="上游代理"
          onValueChange={(value) =>
            setProxyMode(value as GatewayStatus["upstream_proxy_mode"])
          }
          options={[
            { value: "system", label: "系统/环境代理（推荐）" },
            { value: "manual", label: "手动代理" },
            { value: "disabled", label: "直连" },
          ]}
          value={proxyMode}
        />
      </label>
      {proxyMode === "manual" && (
        <label>
          手动代理 URL
          <input
            value={proxyUrl}
            onChange={(event) => setProxyUrl(event.target.value)}
            placeholder={
              gateway.upstream_proxy_display
                ? `已保存：${gateway.upstream_proxy_display}；留空则保留`
                : "例如 http://127.0.0.1:7890 或 socks5h://127.0.0.1:7890"
            }
          />
        </label>
      )}
      {gateway.upstream_last_error && (
        <div className="config-note warning">
          <ShieldCheck size={19} weight="fill" />
          <p>最近上游失败：{gateway.upstream_last_error}</p>
        </div>
      )}
      <div className="config-note">
        <ShieldCheck size={19} weight="fill" />
        <p>
          {bindMode === "loopback"
            ? "仅本机模式只接受 127.0.0.1 访问；每个请求仍必须携带 Relay Client Key。"
            : "留空 CIDR 时，监听网段内任意设备都可连接；每个请求仍必须携带 Relay Client Key。"}
        </p>
      </div>
      <div className="form-actions">
        <span className="muted-copy">服务地址：{gateway.service_url}</span>
        <button
          className="primary-button"
          disabled={busy || gateway.running}
          type="submit"
        >
          保存配置
        </button>
      </div>
    </form>
  );
}

function SecretOnce({
  name,
  value,
  onClose,
}: {
  name: string;
  value: string;
  onClose: () => void;
}) {
  return (
    <section className="secret-once" role="status">
      <div>
        <span className="status-pill warning">
          <i /> 敏感凭据
        </span>
        <h2>{name} 的 Client Key</h2>
        <p>
          请使用受信任的密码管理器手动保存此值。用户管理的 Key
          可在确认后再次查看或轮换，应用不会写入剪贴板。
        </p>
        <code>{value}</code>
      </div>
      <div className="secret-actions">
        <button className="primary-button" type="button" onClick={onClose}>
          我已安全保存
        </button>
      </div>
    </section>
  );
}
