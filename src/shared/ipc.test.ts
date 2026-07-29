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

  it("guides local state read failures without using the generic internal recovery", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockRejectedValueOnce({
      code: "local_state_unavailable",
      message: "本机会话状态暂不可读",
    });

    await expect(relayInvoke("list_codex_sessions")).rejects.toEqual(
      new RelayError(
        "local_state_unavailable",
        "本机会话状态暂不可读（错误码：local_state_unavailable）请在协作页刷新机器人连接；若仍出现，请重启 Codex Relay 并保留该错误码。",
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

  it("preserves raw Tauri string errors instead of replacing them with a generic internal message", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockRejectedValueOnce(
      "模型目录响应读取失败：unexpected token at line 1",
    );

    await expect(relayInvoke("test_api_service_profile")).rejects.toEqual(
      new RelayError(
        "internal",
        "模型目录响应读取失败：unexpected token at line 1（错误码：internal）请重启 Codex Relay；若仍出现，请保留该错误码后重试。",
      ),
    );
  });
});
