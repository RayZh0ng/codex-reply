import {
  Key,
  LockKey,
  Play,
  Plus,
  Power,
  ShieldCheck,
  Trash,
} from "@phosphor-icons/react";
import { FormEvent, useEffect, useState } from "react";

import {
  api,
  type CreatedClientKey,
  type GatewayStatus,
  type MaskedClientKey,
} from "../../shared/ipc";

interface GatewayProps {
  gateway: GatewayStatus;
  busy: boolean;
  onSave: (input: Record<string, unknown>) => Promise<void>;
  onStart: () => Promise<void>;
  onStop: () => Promise<void>;
  onNotice: (message: string) => void;
}

export function Gateway({
  gateway,
  busy,
  onSave,
  onStart,
  onStop,
  onNotice,
}: GatewayProps) {
  const [keys, setKeys] = useState<MaskedClientKey[]>([]);
  const [newKey, setNewKey] = useState<CreatedClientKey | null>(null);
  const [keyName, setKeyName] = useState("");
  const [loadingKeys, setLoadingKeys] = useState(true);
  const reloadKeys = async () => {
    setLoadingKeys(true);
    try {
      setKeys(await api.listClientKeys());
    } catch {
      setKeys([]);
    } finally {
      setLoadingKeys(false);
    }
  };
  useEffect(() => {
    void reloadKeys();
  }, []);
  const createKey = async () => {
    const created = await api.createClientKey(keyName);
    setNewKey(created);
    setKeyName("");
    await reloadKeys();
  };
  const revoke = async (id: string) => {
    await api.revokeClientKey(id);
    await reloadKeys();
    onNotice("客户端 Key 已撤销，受影响客户端需要使用新 Key。");
  };
  return (
    <div className="page gateway-page">
      <header className="page-heading" data-animate="heading">
        <div>
          <p className="section-kicker">Gateway</p>
          <h1>安全的本机服务网关</h1>
          <p className="page-subtitle">
            所有入口均使用 HTTPS。默认只监听回环地址，绝不开放公网。
          </p>
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
          <article className="surface-card security-card">
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
                <strong>{gateway.bind_mode === "lan" ? "受控局域网" : "仅本机"}</strong>
              </li>
              <li>
                <span>客户端鉴权</span>
                <strong>{gateway.client_key_count} 个有效 Key</strong>
              </li>
            </ul>
          </article>
          <article className="surface-card key-card">
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
                      <span>{key.masked_value}</span>
                    </div>
                    <button
                      className="icon-button danger"
                      type="button"
                      aria-label={`撤销 ${key.name}`}
                      onClick={() => void revoke(key.id)}
                    >
                      <Trash size={17} />
                    </button>
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

function GatewayForm({
  gateway,
  busy,
  onSave,
}: {
  gateway: GatewayStatus;
  busy: boolean;
  onSave: (input: Record<string, unknown>) => Promise<void>;
}) {
  const [mode, setMode] = useState(gateway.bind_mode);
  const [address, setAddress] = useState(gateway.bind_address);
  const [port, setPort] = useState(String(gateway.port));
  const [cidrs, setCidrs] = useState(gateway.cidrs.join(", "));
  useEffect(() => {
    setMode(gateway.bind_mode);
    setAddress(gateway.bind_address);
    setPort(String(gateway.port));
    setCidrs(gateway.cidrs.join(", "));
  }, [gateway]);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    await onSave({
      bind_mode: mode,
      bind_address: address,
      port: Number(port),
      cidrs:
        mode === "lan"
          ? cidrs
              .split(",")
              .map((item) => item.trim())
              .filter(Boolean)
          : [],
      confirmed_lan: mode === "lan",
    });
  };
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
      <fieldset>
        <legend>监听模式</legend>
        <div className="segmented">
          <button
            className={mode === "loopback" ? "selected" : ""}
            type="button"
            onClick={() => {
              setMode("loopback");
              setAddress("127.0.0.1");
            }}
          >
            仅本机
          </button>
          <button
            className={mode === "lan" ? "selected" : ""}
            type="button"
            onClick={() => setMode("lan")}
          >
            局域网
          </button>
        </div>
      </fieldset>
      <label>
        监听地址
        <input
          value={address}
          onChange={(event) => setAddress(event.target.value)}
          inputMode="url"
        />
      </label>
      <label>
        服务端口
        <input
          value={port}
          onChange={(event) => setPort(event.target.value)}
          inputMode="numeric"
        />
      </label>
      {mode === "lan" && (
        <label>
          允许的 CIDR
          <input
            required
            value={cidrs}
            onChange={(event) => setCidrs(event.target.value)}
            placeholder="例如：192.168.1.0/24"
          />
        </label>
      )}
      <div className="config-note">
        <ShieldCheck size={19} weight="fill" />
        <p>
          {mode === "lan"
            ? "局域网模式仅允许私有地址和至少一条 CIDR 白名单；客户端必须使用 Bearer Key。"
            : "回环模式拒绝非本机访问，也不会读取代理转发头。"}
        </p>
      </div>
      <div className="form-actions">
        <span className="muted-copy">Base URL：HTTPS 本地证书</span>
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
          <i /> 仅显示一次
        </span>
        <h2>{name} 已创建</h2>
        <p>
          请使用受信任的密码管理器手动保存此值。关闭后不能再次查看原始值，应用不会写入剪贴板。
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
