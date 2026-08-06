import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

import type { GatewayCodexConfigStatus, GatewayStatus } from "../../shared/ipc";
import { api } from "../../shared/ipc";
import { Gateway } from "./Gateway";

vi.mock("@tauri-apps/plugin-dialog", () => ({ save: vi.fn() }));
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

vi.mock("../../shared/ipc", () => ({
  api: {
    listClientKeys: vi.fn().mockResolvedValue([]),
    codexGatewayConfigStatus: vi.fn().mockResolvedValue({
      enabled: false,
      mode: "official",
      config_path: "/Users/test/.codex/config.toml",
      service_url: null,
      message: "Codex 尚未切换到 Relay 网关。",
      auth_status: "missing",
      needs_repair: false,
      direct_profile_id: null,
      direct_profile_alias: null,
      oauth_profile_id: null,
      oauth_profile_alias: null,
      oauth_profile_available: false,
      oauth_profile_options: [],
      history_sync: null,
    }),
    createClientKey: vi.fn(),
    revealClientKey: vi.fn(),
    rotateClientKey: vi.fn(),
    revokeClientKey: vi.fn(),
    exportGatewayCa: vi.fn(),
    trustGatewayCa: vi.fn(),
    enableCodexGateway: vi.fn(),
    disableCodexGateway: vi.fn(),
    setCodexGatewayOAuthProfile: vi.fn(),
  },
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const gateway: GatewayStatus = {
  running: true,
  bind_mode: "lan",
  bind_address: "10.12.14.248",
  available_addresses: [],
  port: 53765,
  cidrs: [],
  available_profiles: 0,
  cooling_profiles: 0,
  client_key_count: 1,
  certificate_ready: true,
  service_url: "https://10.12.14.248:53765",
  upstream_proxy_mode: "system",
  upstream_proxy_display: null,
  upstream_last_error: null,
};

function renderGateway(overrides: Partial<GatewayStatus> = {}) {
  const onNavigateProfiles = vi.fn();
  const onRefresh = vi.fn().mockResolvedValue(undefined);
  const onNotice = vi.fn();
  render(
    <Gateway
      gateway={{ ...gateway, ...overrides }}
      busy={false}
      onSave={vi.fn().mockResolvedValue(undefined)}
      onStart={vi.fn().mockResolvedValue(undefined)}
      onStop={vi.fn().mockResolvedValue(undefined)}
      onNotice={onNotice}
      onNavigateProfiles={onNavigateProfiles}
      onRefresh={onRefresh}
    />,
  );
  return { onNavigateProfiles, onRefresh, onNotice };
}

describe("Gateway", () => {
  it("keeps the current bind address visible when address detection is empty", () => {
    renderGateway();

    expect(
      screen.getByRole("combobox", { name: "检测到的局域网地址" }),
    ).toHaveTextContent("当前地址 · 10.12.14.248");
  });

  it("guides users to profiles when no gateway account members are available", () => {
    const { onNavigateProfiles } = renderGateway();

    expect(screen.getByText("当前没有网关账号成员")).toBeInTheDocument();
    expect(screen.getByText(/服务已启动，但还没有可用账号成员/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "去档案加入网关" }));
    expect(onNavigateProfiles).toHaveBeenCalledOnce();
    expect(api.listClientKeys).toHaveBeenCalled();
  });

  it("hides the empty member guide once a gateway member is available", () => {
    renderGateway({ available_profiles: 1 });

    expect(screen.queryByText("当前没有网关账号成员")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "去档案加入网关" }),
    ).not.toBeInTheDocument();
  });

  it("marks codex-managed keys and hides their revoke action", async () => {
    vi.mocked(api.listClientKeys).mockResolvedValue([
      {
        id: "codex",
        name: "Codex CLI Gateway",
        masked_value: "crl_••••c0dx",
        created_at_ms: 1,
        last_used_at_ms: null,
        revoked: false,
        managed_by: "codex_gateway",
        can_revoke: false,
      },
      {
        id: "user",
        name: "Alice 的 Mac",
        masked_value: "crl_••••user",
        created_at_ms: 2,
        last_used_at_ms: null,
        revoked: false,
        managed_by: "user",
        can_revoke: true,
      },
    ]);

    renderGateway({ available_profiles: 1 });

    expect(await screen.findByText("Codex 自动管理")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "撤销 Codex CLI Gateway" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "撤销 Alice 的 Mac" }),
    ).toBeInTheDocument();
  });

  it("removes a revoked user key from the list and refreshes the parent gateway count", async () => {
    vi.mocked(api.listClientKeys)
      .mockResolvedValueOnce([
        {
          id: "user",
          name: "Alice 的 Mac",
          masked_value: "crl_••••user",
          created_at_ms: 2,
          last_used_at_ms: null,
          revoked: false,
          managed_by: "user",
          can_revoke: true,
        },
      ])
      .mockResolvedValue([]);
    vi.mocked(api.revokeClientKey).mockResolvedValue(undefined);
    const { onRefresh } = renderGateway({ available_profiles: 1 });

    fireEvent.click(await screen.findByRole("button", { name: "撤销 Alice 的 Mac" }));

    await waitFor(() => expect(api.revokeClientKey).toHaveBeenCalledWith("user"));
    await waitFor(() => expect(onRefresh).toHaveBeenCalledOnce());
    expect(screen.queryByText("Alice 的 Mac")).not.toBeInTheDocument();
  });

  it("uses backend oauth profile options and unavailable reasons", async () => {
    vi.mocked(api.codexGatewayConfigStatus).mockResolvedValueOnce({
      enabled: true,
      mode: "relay_gateway",
      message: "Codex 已切换到 Relay 网关。",
      auth_status: "ok",
      needs_repair: false,
      config_path: "/Users/test/.codex/config.toml",
      service_url: "https://10.12.14.248:53765",
      direct_profile_id: null,
      direct_profile_alias: null,
      oauth_profile_id: "oauth-work",
      oauth_profile_alias: "工作 OAuth",
      oauth_profile_available: true,
      oauth_profile_options: [
        {
          id: "oauth-work",
          alias: "工作 OAuth",
          available: true,
          reason: null,
        },
        {
          id: "oauth-missing",
          alias: "未授权 OAuth",
          available: false,
          reason: "凭据未保存，请重新授权",
        },
        {
          id: "json-import",
          alias: "JSON 导入账号",
          available: false,
          reason: "JSON 导入账号用于反代账号池，不能用于登录态解锁",
        },
      ],
      history_sync: null,
    });

    renderGateway({ available_profiles: 1 });

    expect(await screen.findByText(/当前登录档案：工作 OAuth/)).toBeInTheDocument();
    const trigger = screen.getByRole("combobox", { name: "OAuth 登录档案" });
    expect(trigger).toHaveTextContent("工作 OAuth");

    fireEvent.click(trigger);
    expect(await screen.findByText("凭据未保存，请重新授权")).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /未授权 OAuth/ })).toHaveAttribute(
      "data-disabled",
    );
    expect(
      screen.getByText("JSON 导入账号用于反代账号池，不能用于登录态解锁"),
    ).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /JSON 导入账号/ })).toHaveAttribute(
      "data-disabled",
    );
  });

  it("reads and saves the direct profile OAuth binding with mode-specific feedback", async () => {
    const directStatus = {
      enabled: true,
      mode: "third_party",
      message: "Codex 已切换到第三方模型提供商直连。",
      auth_status: "ok",
      needs_repair: false,
      config_path: "/Users/test/.codex/config.toml",
      service_url: "https://api.example.com/v1",
      direct_profile_id: "api-direct",
      direct_profile_alias: "第三方供应商",
      oauth_profile_id: "oauth-a",
      oauth_profile_alias: "账号 A",
      oauth_profile_available: true,
      oauth_profile_options: [
        { id: "oauth-a", alias: "账号 A", available: true, reason: null },
        { id: "oauth-b", alias: "账号 B", available: true, reason: null },
      ],
      history_sync: null,
    } satisfies GatewayCodexConfigStatus;
    vi.mocked(api.codexGatewayConfigStatus).mockResolvedValueOnce(directStatus);
    vi.mocked(api.setCodexGatewayOAuthProfile).mockResolvedValue({
      ...directStatus,
      oauth_profile_id: "oauth-b",
      oauth_profile_alias: "账号 B",
      message:
        "OAuth 登录档案已写入并验证为所选账号；模型请求仍走第三方提供商。已重启 Codex。",
    });
    const { onNotice } = renderGateway({ available_profiles: 1 });

    const trigger = await screen.findByRole("combobox", { name: "OAuth 登录档案" });
    expect(trigger).toHaveTextContent("账号 A");
    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole("option", { name: "账号 B" }));

    await waitFor(() =>
      expect(api.setCodexGatewayOAuthProfile).toHaveBeenCalledWith("oauth-b"),
    );
    expect(onNotice).toHaveBeenCalledWith(
      "OAuth 登录档案已写入并验证为所选账号；模型请求仍走第三方提供商。已重启 Codex。",
    );
    expect(trigger).toHaveTextContent("账号 B");
  });

  it("reveals and rotates user-managed client keys", async () => {
    const userKey = {
      id: "user",
      name: "Alice 的 Mac",
      masked_value: "crl_••••user",
      created_at_ms: 2,
      last_used_at_ms: null,
      revoked: false,
      managed_by: "user" as const,
      can_revoke: true,
    };
    vi.mocked(api.listClientKeys).mockResolvedValue([userKey]);
    vi.mocked(api.revealClientKey).mockResolvedValue("crl_plain_user");
    vi.mocked(api.rotateClientKey).mockResolvedValue({
      key: { ...userKey, masked_value: "crl_••••next" },
      plaintext_once: "crl_plain_next",
    });
    const { onRefresh } = renderGateway({ available_profiles: 1 });

    fireEvent.click(await screen.findByRole("button", { name: "查看 Alice 的 Mac" }));

    await waitFor(() => expect(api.revealClientKey).toHaveBeenCalledWith("user"));
    expect(await screen.findByText("crl_plain_user")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "我已安全保存" }));
    fireEvent.click(screen.getByRole("button", { name: "轮换 Alice 的 Mac" }));

    await waitFor(() => expect(api.rotateClientKey).toHaveBeenCalledWith("user"));
    expect(await screen.findByText("crl_plain_next")).toBeInTheDocument();
    await waitFor(() => expect(onRefresh).toHaveBeenCalledOnce());
  });

  it("saves loopback mode without LAN address or CIDR", async () => {
    const onSave = vi.fn().mockResolvedValue(undefined);
    render(
      <Gateway
        gateway={{
          ...gateway,
          running: false,
          bind_mode: "loopback",
          bind_address: "127.0.0.1",
          service_url: "https://127.0.0.1:53765",
        }}
        busy={false}
        onSave={onSave}
        onStart={vi.fn().mockResolvedValue(undefined)}
        onStop={vi.fn().mockResolvedValue(undefined)}
        onNotice={vi.fn()}
        onNavigateProfiles={vi.fn()}
        onRefresh={vi.fn().mockResolvedValue(undefined)}
      />,
    );

    expect(
      screen.queryByRole("combobox", { name: "检测到的局域网地址" }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "保存配置" }));

    await waitFor(() =>
      expect(onSave).toHaveBeenCalledWith(
        expect.objectContaining({
          bind_mode: "loopback",
          bind_address: "127.0.0.1",
          cidrs: [],
          confirmed_lan: false,
        }),
      ),
    );
  });

  it("repairs codex key and refreshes gateway state", async () => {
    vi.mocked(api.codexGatewayConfigStatus).mockResolvedValue({
      enabled: true,
      mode: "relay_gateway",
      message: "Codex Relay 网关 Client Key 已失效。",
      auth_status: "invalid",
      needs_repair: true,
      config_path: "/Users/test/.codex/config.toml",
      service_url: null,
      direct_profile_id: null,
      direct_profile_alias: null,
      oauth_profile_id: null,
      oauth_profile_alias: null,
      oauth_profile_available: false,
      oauth_profile_options: [],
      history_sync: null,
    });
    vi.mocked(api.enableCodexGateway).mockResolvedValue({
      enabled: true,
      mode: "relay_gateway",
      message: "Codex 已切换到 Relay 网关。",
      auth_status: "ok",
      needs_repair: false,
      config_path: "/Users/test/.codex/config.toml",
      service_url: "https://10.12.14.248:53765",
      direct_profile_id: null,
      direct_profile_alias: null,
      oauth_profile_id: null,
      oauth_profile_alias: null,
      oauth_profile_available: false,
      oauth_profile_options: [],
      history_sync: null,
    });
    const { onRefresh } = renderGateway({ available_profiles: 1 });

    fireEvent.click(await screen.findByRole("button", { name: "修复 Codex 配置" }));

    await waitFor(() => expect(api.enableCodexGateway).toHaveBeenCalledOnce());
    await waitFor(() => expect(onRefresh).toHaveBeenCalledOnce());
    expect(api.listClientKeys).toHaveBeenCalledTimes(2);
  });

  it("allows Codex gateway switching before the gateway has been started", async () => {
    vi.mocked(api.enableCodexGateway).mockResolvedValue({
      enabled: true,
      mode: "relay_gateway",
      message: "Codex 已切换到 Relay 网关，并已复用原客户端状态。",
      auth_status: "ok",
      needs_repair: false,
      config_path: "/Users/test/.codex/config.toml",
      service_url: "https://10.12.14.248:53765",
      direct_profile_id: null,
      direct_profile_alias: null,
      oauth_profile_id: null,
      oauth_profile_alias: null,
      oauth_profile_available: false,
      oauth_profile_options: [],
      history_sync: null,
    });
    const { onRefresh } = renderGateway({
      running: false,
      certificate_ready: false,
      available_profiles: 1,
    });

    const button = await screen.findByRole("button", { name: "设为 Codex 网关" });
    expect(button).toBeEnabled();
    fireEvent.click(button);

    await waitFor(() => expect(api.enableCodexGateway).toHaveBeenCalledOnce());
    await waitFor(() => expect(onRefresh).toHaveBeenCalledOnce());
  });
});
