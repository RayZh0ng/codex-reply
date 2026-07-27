import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { GatewayStatus } from "../../shared/ipc";
import { api } from "../../shared/ipc";
import { Gateway } from "./Gateway";

vi.mock("@tauri-apps/plugin-dialog", () => ({ save: vi.fn() }));
vi.mock("../../shared/ipc", () => ({
  api: {
    listClientKeys: vi.fn().mockResolvedValue([]),
    codexGatewayConfigStatus: vi.fn().mockResolvedValue({
      enabled: false,
      config_path: "/Users/test/.codex/config.toml",
      service_url: null,
      message: "Codex 尚未切换到 Relay 网关。",
      auth_status: "missing",
      needs_repair: false,
    }),
    createClientKey: vi.fn(),
    revokeClientKey: vi.fn(),
    exportGatewayCa: vi.fn(),
    trustGatewayCa: vi.fn(),
    enableCodexGateway: vi.fn(),
    disableCodexGateway: vi.fn(),
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
  render(
    <Gateway
      gateway={{ ...gateway, ...overrides }}
      busy={false}
      onSave={vi.fn().mockResolvedValue(undefined)}
      onStart={vi.fn().mockResolvedValue(undefined)}
      onStop={vi.fn().mockResolvedValue(undefined)}
      onNotice={vi.fn()}
      onNavigateProfiles={onNavigateProfiles}
      onRefresh={onRefresh}
    />,
  );
  return { onNavigateProfiles, onRefresh };
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

  it("repairs codex key and refreshes gateway state", async () => {
    vi.mocked(api.codexGatewayConfigStatus).mockResolvedValue({
      enabled: true,
      message: "Codex Relay 网关 Client Key 已失效。",
      auth_status: "invalid",
      needs_repair: true,
      config_path: "/Users/test/.codex/config.toml",
      service_url: null,
    });
    vi.mocked(api.enableCodexGateway).mockResolvedValue({
      enabled: true,
      message: "Codex 已切换到 Relay 网关。",
      auth_status: "ok",
      needs_repair: false,
      config_path: "/Users/test/.codex/config.toml",
      service_url: "https://10.12.14.248:53765",
    });
    const { onRefresh } = renderGateway({ available_profiles: 1 });

    fireEvent.click(await screen.findByRole("button", { name: "修复 Codex Key" }));

    await waitFor(() => expect(api.enableCodexGateway).toHaveBeenCalledOnce());
    await waitFor(() => expect(onRefresh).toHaveBeenCalledOnce());
    expect(api.listClientKeys).toHaveBeenCalledTimes(2);
  });
});
