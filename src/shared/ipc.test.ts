import { describe, expect, it, vi } from "vitest";

const native = vi.hoisted(() => ({ invoke: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => native);

import { RelayError, relayInvoke } from "./ipc";

describe("relayInvoke", () => {
  it("keeps a safe Rust error code and recovery hint instead of replacing it", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockRejectedValueOnce({
      code: "internal",
      message: "内部状态不可用",
    });

    await expect(relayInvoke("dashboard_snapshot")).rejects.toEqual(
      new RelayError(
        "internal",
        "内部状态不可用（错误码：internal）请重启 Codex Relay；若仍出现，请保留该错误码后重试。",
      ),
    );
  });

  it("guides a user-initiated Keychain authorization without exposing platform errors", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockRejectedValueOnce({
      code: "keychain_interaction_required",
      message: "系统钥匙串需要用户授权",
    });

    await expect(relayInvoke("sync_profile_account_info")).rejects.toEqual(
      new RelayError(
        "keychain_interaction_required",
        "系统钥匙串需要用户授权（错误码：keychain_interaction_required）请在 macOS 系统弹窗中输入登录钥匙串密码，并选择“始终允许”。",
      ),
    );
  });
});
