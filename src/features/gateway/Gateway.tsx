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
  type GatewayRequestMetricPage,
  type CodexHistoryTransitionStatus,
  type MaskedClientKey,
} from "../../shared/ipc";
import {
  Button,
  EmptyState,
  InlineNotice,
  PageHeader,
  Select,
  StatusPill,
} from "../../shared/ui";
import "./gateway.css";

const NO_OAUTH_PROFILE = "__none__";

interface GatewayProps {
  gateway: GatewayStatus;
  busy: boolean;
  onSave: (input: Record<string, unknown>) => Promise<void>;
  onStart: () => Promise<void>;
  onStop: () => Promise<void>;
  onNotice: (message: string) => void;
  onHistorySyncStatus?: (status: CodexHistoryTransitionStatus) => void;
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
  onHistorySyncStatus = () => undefined,
  onNavigateProfiles,
  onRefresh,
}: GatewayProps) {
  const [keys, setKeys] = useState<MaskedClientKey[]>([]);
  const [newKey, setNewKey] = useState<CreatedClientKey | null>(null);
  const [keyName, setKeyName] = useState("");
  const [loadingKeys, setLoadingKeys] = useState(true);
  const [codexConfig, setCodexConfig] = useState<GatewayCodexConfigStatus | null>(null);
  const [requestMetrics, setRequestMetrics] = useState<GatewayRequestMetricPage>({
    items: [],
    next_cursor: null,
  });
  const [loadingMetrics, setLoadingMetrics] = useState(true);
  const codexConfigMode = codexConfig?.mode ?? "official";
  const codexManaged = codexConfigMode !== "official";
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
      description: "保留当前 Codex 登录态，只切换模型请求地址",
    },
    ...oauthCandidates.map((profile) => ({
      value: profile.id,
      label: profile.alias,
      disabled: !profile.available,
      description: profile.available
        ? "只用于解锁 Codex 登录态"
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
  const loadRequestMetrics = useCallback(async (cursor?: number | null) => {
    setLoadingMetrics(true);
    try {
      const page = await api.listGatewayRequestMetrics({
        limit: 25,
        cursor: cursor ?? null,
      });
      if (!page || !Array.isArray(page.items)) {
        throw new Error("invalid gateway metrics response");
      }
      setRequestMetrics((current) =>
        cursor == null
          ? page
          : { items: [...current.items, ...page.items], next_cursor: page.next_cursor },
      );
    } catch {
      if (cursor == null) setRequestMetrics({ items: [], next_cursor: null });
    } finally {
      setLoadingMetrics(false);
    }
  }, []);

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
  useEffect(() => {
    void loadRequestMetrics();
  }, [
    gateway.running,
    gateway.active_requests,
    gateway.queued_requests,
    loadRequestMetrics,
  ]);
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
      if (next.history_sync_status) onHistorySyncStatus(next.history_sync_status);
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
      onNotice(next.message);
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "OAuth 登录档案未保存。");
    }
  };
  const codexButtonLabel = codexConfig?.needs_repair
    ? "修复 Codex 配置"
    : codexManaged
      ? "恢复官方配置"
      : "设为 Codex 网关";
  const codexStatusLabel = codexConfig?.needs_repair
    ? "需要修复 Codex 配置"
    : codexConfigMode === "third_party"
      ? "第三方直连"
      : codexConfigMode === "relay_gateway"
        ? "已接入 Relay 网关"
        : "官方模型配置";
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
      <PageHeader
        actions={
          <Button
            disabled={busy}
            leadingIcon={
              gateway.running ? (
                <Power size={18} weight="bold" />
              ) : (
                <Play size={18} weight="fill" />
              )
            }
            variant={gateway.running ? "danger" : "primary"}
            onClick={() => void (gateway.running ? onStop() : onStart())}
          >
            {gateway.running ? "停止服务" : "启动 API 服务"}
          </Button>
        }
        description="所有入口均使用 HTTPS。可限制为 127.0.0.1，也可绑定检测到的私有局域网地址；始终需要客户端 Key。"
        meta={
          <div className="gateway-heading-meta">
            <code>{gateway.service_url}</code>
            <span>{gateway.available_profiles} 个可用 API 成员</span>
          </div>
        }
        title="本机 / 局域网 API 网关"
      />
      <GatewayStatusSummary gateway={gateway} />
      <GatewayRequestMetrics
        loading={loadingMetrics}
        page={requestMetrics}
        onLoadMore={(cursor) => void loadRequestMetrics(cursor)}
      />
      {gateway.available_profiles === 0 && gateway.direct_route?.status !== "ok" && (
        <InlineNotice
          action={
            <Button size="sm" variant="primary" onClick={onNavigateProfiles}>
              去档案加入网关
            </Button>
          }
          aria-label="网关账号成员提示"
          tone="warning"
          title="当前没有网关账号成员"
        >
          {gateway.running
            ? "服务已启动，但还没有可用账号成员。请到档案页刷新模型并把账号加入网关。"
            : "启动前请先到档案页刷新模型并把账号加入网关；也可先完成监听与证书配置。"}
        </InlineNotice>
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
                <p className="section-kicker">访问保护</p>
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
                <p className="section-kicker">Codex 配置</p>
                <h2>Codex 网关切换</h2>
              </div>
              <LockKey size={23} weight="duotone" />
            </div>
            <div className="codex-config-overview">
              <span
                className={`codex-config-status ${
                  codexManaged && !codexConfig?.needs_repair ? "" : "is-disabled"
                }`}
              >
                {codexStatusLabel}
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
                  codexManaged && !codexConfig?.needs_repair
                    ? "quiet-button"
                    : "primary-button"
                }`}
                disabled={busy || !codexConfig}
                type="button"
                onClick={() => void toggleCodexGateway()}
              >
                {codexButtonLabel}
              </button>
            </div>
            <p className="codex-config-helper">
              {codexConfigMode === "third_party"
                ? codexConfig?.oauth_profile_id && codexConfig.oauth_profile_available
                  ? `当前第三方：${codexConfig.direct_profile_alias ?? codexConfig.direct_profile_id ?? "第三方供应商"}；ChatGPT 使用所选 OAuth 登录，模型请求通过 codex_relay_direct 经本机 Relay 固定转发，OAuth Token 不会发送给第三方。`
                  : `当前直连：${codexConfig?.direct_profile_alias ?? codexConfig?.direct_profile_id ?? "第三方供应商"}；OAuth 未应用时保留当前 ChatGPT 登录，模型请求仍直接发送到该供应商 Base URL。`
                : codexConfig?.oauth_profile_id
                  ? codexConfig.oauth_profile_available
                    ? `当前登录档案：${codexConfig.oauth_profile_alias ?? codexConfig.oauth_profile_id}；只解锁 Codex 登录态，模型请求继续走 Relay 网关账号池。`
                    : (selectedOAuthOption?.reason ??
                      "已绑定的 OAuth 登录档案当前不可读，请重新授权或清空绑定。")
                  : hasAvailableOAuthCandidate
                    ? "未绑定时保留本机登录档案；切换后模型请求继续走 Relay 网关账号池。"
                    : "还没有可用于登录态解锁的 OAuth 授权档案；JSON/PAT/Agent Identity 档案只用于反代账号池。"}
            </p>
          </article>
          <article className="surface-card key-card gateway-card">
            <div className="card-heading">
              <div>
                <p className="section-kicker">客户端访问</p>
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
                    <div className="key-info">
                      <span className="key-title-row">
                        <strong>{key.name}</strong>
                        {key.managed_by === "codex_gateway" && (
                          <span className="managed-key-badge">Codex 自动管理</span>
                        )}
                      </span>
                      <code className="key-mask">{key.masked_value}</code>
                    </div>
                    <div
                      aria-hidden={key.can_revoke ? undefined : true}
                      className={
                        key.can_revoke ? "key-actions" : "key-actions is-empty"
                      }
                    >
                      {key.can_revoke && (
                        <>
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
                        </>
                      )}
                    </div>
                  </li>
                ))}
              </ul>
            ) : (
              <EmptyState
                compact
                description="创建后只向受信任客户端分发；每个请求都必须携带 Key。"
                icon={<Key size={19} />}
                title="还没有客户端 Key"
              />
            )}
          </article>
        </aside>
      </section>
    </div>
  );
}

function GatewayRequestMetrics({
  loading,
  page,
  onLoadMore,
}: {
  loading: boolean;
  page: GatewayRequestMetricPage;
  onLoadMore: (cursor: number) => void;
}) {
  return (
    <section className="gateway-request-metrics" aria-label="最近网关请求">
      <div className="gateway-panel-heading">
        <div>
          <h2>最近请求</h2>
          <p>仅保存脱敏路由、耗时、状态、字节数与 usage，不保存提示词或响应正文。</p>
        </div>
        <StatusPill
          tone={
            page.items.some((item) => item.outcome !== "success")
              ? "warning"
              : "neutral"
          }
        >
          {page.items.length} 条
        </StatusPill>
      </div>
      {page.items.length === 0 ? (
        <p className="gateway-metric-empty">
          {loading ? "正在读取请求指标…" : "暂无请求指标。"}
        </p>
      ) : (
        <div className="gateway-metric-table-wrap">
          <table className="gateway-metric-table">
            <thead>
              <tr>
                <th>时间</th>
                <th>路由</th>
                <th>状态</th>
                <th>TTFB / 总耗时</th>
                <th>队列</th>
                <th>尝试</th>
              </tr>
            </thead>
            <tbody>
              {page.items.map((item) => (
                <tr key={item.request_id}>
                  <td>{new Date(item.started_at_ms).toLocaleTimeString()}</td>
                  <td>
                    <code>{item.route}</code>
                    <small>
                      {item.provider} · {item.auth_mode}
                      {item.stream ? " · SSE" : ""}
                    </small>
                  </td>
                  <td>
                    <StatusPill
                      tone={item.outcome === "success" ? "success" : "danger"}
                    >
                      {item.http_status}
                    </StatusPill>
                    <small>{item.error_category ?? item.outcome}</small>
                  </td>
                  <td>
                    {item.ttfb_ms == null ? "—" : `${item.ttfb_ms} ms`} /{" "}
                    {item.total_latency_ms} ms
                  </td>
                  <td>{item.queue_latency_ms} ms</td>
                  <td>
                    {item.upstream_attempts}
                    {item.retry_count ? ` · ${item.retry_count} retry` : ""}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {page.next_cursor != null && (
        <Button
          disabled={loading}
          size="sm"
          variant="secondary"
          onClick={() => onLoadMore(page.next_cursor!)}
        >
          {loading ? "加载中…" : "加载更多"}
        </Button>
      )}
    </section>
  );
}

function GatewayStatusSummary({ gateway }: { gateway: GatewayStatus }) {
  return (
    <section className="gateway-status-summary" aria-label="网关状态摘要">
      <div className="gateway-status-main">
        <StatusPill tone={gateway.running ? "success" : "neutral"}>
          {gateway.running ? "服务运行中" : "服务未启动"}
        </StatusPill>
        <code className="gateway-endpoint">{gateway.service_url}</code>
      </div>
      <div className="gateway-status-actions" aria-label="网关关键状态">
        <span className="gateway-status-meta">
          账号池{" "}
          {gateway.pool_status ??
            (gateway.available_profiles > 0 ? "ok" : "unavailable")}{" "}
          · {gateway.available_profiles} 个成员
        </span>
        <span className="gateway-status-meta">
          Direct {gateway.direct_route?.status ?? "unavailable"}
          {gateway.direct_route?.profile_alias
            ? ` · ${gateway.direct_route.profile_alias}`
            : ""}
        </span>
        <span className="gateway-status-meta">
          {gateway.active_requests ?? 0} 活动 · {gateway.queued_requests ?? 0} 排队
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
          <p className="section-kicker">服务配置</p>
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
