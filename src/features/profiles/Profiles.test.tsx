import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { open } from "@tauri-apps/plugin-dialog";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

const webview = vi.hoisted(() => ({
  dragHandler: null as
    ((event: { payload: { type: string; paths: string[] } }) => void) | null,
  onDragDropEvent: vi.fn(),
  unlisten: vi.fn(),
}));

import type {
  JsonProfileImportPreview,
  MaskedProfile,
  OAuthImportStatus,
  ProfileAccountSummary,
  ProfileQuotaWindow,
} from "../../shared/ipc";
import { api } from "../../shared/ipc";
import { Profiles } from "./Profiles";

vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ onDragDropEvent: webview.onDragDropEvent }),
}));

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const status = {
  attempt_id: "attempt-1",
  profile_id: null,
  phase: "authorizing" as const,
  message: "已在默认浏览器中打开 OpenAI / ChatGPT 登录页面。",
};

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.clearAllMocks();
  vi.restoreAllMocks();
  webview.dragHandler = null;
  delete (window as typeof window & { __TAURI_INTERNALS__?: unknown })
    .__TAURI_INTERNALS__;
});

function renderProfiles(profiles: MaskedProfile[] = []) {
  const onStartOAuth = vi.fn().mockResolvedValue(status);
  return {
    ...render(
      <Profiles
        profiles={profiles}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={onStartOAuth}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    ),
    onStartOAuth,
  };
}

function quotaWindow(
  remainingPercent: number,
  resetsAtMs: number | null,
): ProfileQuotaWindow {
  return {
    used_percent: 100 - remainingPercent,
    remaining_percent: remainingPercent,
    window_duration_mins: 300,
    resets_at_ms: resetsAtMs,
  };
}

function accountSummary({
  email,
  planType,
  periodEndsAtMs = null,
  windows = [],
}: {
  email: string | null;
  planType: string | null;
  periodEndsAtMs?: number | null;
  windows?: ProfileQuotaWindow[];
}): ProfileAccountSummary {
  const [primary, secondary, extra] = windows;
  return {
    display_name: null,
    email,
    account_id: null,
    updated_at_ms: 0,
    quota: {
      status: "available",
      message: "已同步 Codex 额度。",
      source: "app_server",
      synced_at_ms: 0,
      last_attempt_at_ms: 0,
      last_error: null,
      primary: null,
      secondary: null,
      buckets: windows.length
        ? [
            {
              id: "codex",
              name: "Codex",
              plan_type: planType,
              primary: primary ?? null,
              secondary: secondary ?? null,
            },
            ...(extra
              ? [
                  {
                    id: "extra",
                    name: "Extra",
                    plan_type: planType,
                    primary: extra,
                    secondary: null,
                  },
                ]
              : []),
          ]
        : [],
      rate_limit_reached_type: null,
    },
    subscription: {
      status: "available",
      plan_type: planType,
      period_ends_at_ms: periodEndsAtMs,
      will_renew: true,
      source: "app_server",
      synced_at_ms: 0,
      last_attempt_at_ms: 0,
      last_error: null,
    },
  };
}

function profileFixture({
  id,
  alias,
  account = null,
  kind = "codex_oauth",
  authMode,
  validationStatus = "unknown",
  validatedAtMs = null,
  validationMessage = null,
}: {
  id: string;
  alias: string;
  account?: ProfileAccountSummary | null;
  kind?: MaskedProfile["kind"];
  authMode?: MaskedProfile["auth_mode"];
  validationStatus?: MaskedProfile["validation_status"];
  validatedAtMs?: number | null;
  validationMessage?: string | null;
}): MaskedProfile {
  return {
    id,
    alias,
    kind,
    base_url: null,
    enabled: true,
    in_pool: false,
    priority: 0,
    weight: 1,
    models: [],
    codex_oauth_profile_id: null,
    health: "unknown",
    cooldown_until_ms: null,
    credential_configured: true,
    is_current: false,
    validation_status: validationStatus,
    validated_at_ms: validatedAtMs,
    validation_message: validationMessage,
    auth_mode: authMode,
    account,
  };
}

function profileAliases() {
  return screen
    .getAllByRole("heading", { level: 2 })
    .map((heading) => heading.textContent);
}

function chooseMenuOption(label: string, currentValue: string, optionLabel: string) {
  fireEvent.click(screen.getByRole("button", { name: `${label}：${currentValue}` }));
  fireEvent.click(
    within(screen.getByRole("listbox", { name: `${label}选项` })).getByRole("option", {
      name: optionLabel,
    }),
  );
}

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, reject, resolve };
}

describe("Profiles OAuth import", () => {
  it("keeps profiles in a responsive, unified list surface", () => {
    const view = renderProfiles([
      profileFixture({ id: "work", alias: "Work OAuth" }),
      profileFixture({ id: "personal", alias: "Personal OAuth" }),
    ]);

    const grid = view.container.querySelector(".profiles-page .profile-grid");
    const cards = view.container.querySelectorAll(".profiles-page .profile-card");

    expect(grid).toBeInTheDocument();
    expect(cards).toHaveLength(2);
    expect(cards[0]).toContainElement(screen.getByText("Work OAuth"));
    expect(cards[1]).toContainElement(screen.getByText("Personal OAuth"));
    expect(view.container.querySelectorAll(".profile-actions")).toHaveLength(2);
  });

  it("keeps the current profile badge as a single card label", () => {
    const view = renderProfiles([
      {
        ...profileFixture({
          id: "current",
          alias: "Current OAuth",
          account: accountSummary({
            email: "very-long-current-profile-address@example.com",
            planType: "plus",
          }),
        }),
        is_current: true,
      },
    ]);

    const badges = view.container.querySelectorAll(".current-badge");
    expect(badges).toHaveLength(1);
    expect(badges[0].textContent?.trim()).toBe("当前");
  });

  it("keeps the empty profile state in the profile list surface", () => {
    const view = renderProfiles();
    const emptyState = view.container.querySelector(".profile-grid > .empty-state");

    expect(emptyState).toBeInTheDocument();
    expect(emptyState).toHaveTextContent("从一个已获授权的连接开始");
  });

  it("shows invalid profile details and starts reauthorization from the status card", async () => {
    const view = renderProfiles([
      profileFixture({
        id: "invalid",
        alias: "失效账号",
        validationStatus: "invalid",
        validatedAtMs: 1_700_000_000_000,
        validationMessage: "官方 Codex 接口拒绝了当前登录凭据，请重新授权。",
      }),
    ]);

    const validation = screen.getByLabelText("档案有效性：失效账号");
    expect(validation).toHaveTextContent("档案已失效");
    expect(validation).toHaveTextContent("官方 Codex 接口拒绝了当前登录凭据");
    expect(validation).toHaveTextContent("上次验证");

    fireEvent.click(within(validation).getByRole("button", { name: "重新授权" }));
    await waitFor(() => expect(view.onStartOAuth).toHaveBeenCalledWith("invalid"));
  });

  it("does not keep polling and show a stale status error after successful reauthorization", async () => {
    vi.useFakeTimers();
    const authorizingStatus: OAuthImportStatus = {
      ...status,
      profile_id: "oauth-profile",
    };
    const authenticatedStatus: OAuthImportStatus = {
      ...authorizingStatus,
      phase: "authenticated",
      message: "授权完成，凭据将在创建档案时保存。",
    };
    const completion = deferred<void>();
    const onStartOAuth = vi.fn().mockResolvedValue(authorizingStatus);
    const onOAuthStatus = vi.fn().mockResolvedValue(authenticatedStatus);
    const onCompleteOAuth = vi.fn().mockReturnValue(completion.promise);

    render(
      <Profiles
        profiles={[profileFixture({ id: "oauth-profile", alias: "Personal OAuth" })]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={onStartOAuth}
        onOAuthStatus={onOAuthStatus}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={onCompleteOAuth}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "更新凭据：Personal OAuth" }));
    await act(async () => undefined);

    expect(screen.getByText("等待默认浏览器授权完成")).toBeInTheDocument();

    await act(async () => {
      vi.advanceTimersByTime(1_000);
    });

    expect(onOAuthStatus).toHaveBeenCalledTimes(1);
    expect(onCompleteOAuth).toHaveBeenCalledWith("attempt-1");

    onOAuthStatus.mockRejectedValue(new Error("attempt already completed"));

    await act(async () => {
      vi.advanceTimersByTime(3_000);
    });

    expect(onOAuthStatus).toHaveBeenCalledTimes(1);
    expect(
      screen.queryByText("无法读取授权状态；请取消后重试。"),
    ).not.toBeInTheDocument();

    completion.resolve();
    await act(async () => {
      await completion.promise;
    });

    expect(
      screen.queryByRole("dialog", { name: "选择导入方式" }),
    ).not.toBeInTheDocument();
  });

  it("states that profile switching uses saved credentials without reauthorization", () => {
    renderProfiles();

    expect(screen.getByText("当前档案会复用原 Codex 客户端状态")).toBeInTheDocument();
    expect(
      screen.getByText(/添加账号时会保存认证凭据。切换账号会更新默认/),
    ).toBeInTheDocument();
    expect(screen.getByText(/保留本机聊天记录、记忆、设置与状态/)).toBeInTheDocument();
  });

  it("opens distinct OAuth and JSON import cards from the page action", () => {
    const view = renderProfiles();

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);

    expect(screen.getByText("使用 OpenAI / ChatGPT 登录")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "继续使用 OAuth" })).toBeInTheDocument();
    expect(screen.getByText("从 JSON 文件导入")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "选择 JSON 文件" })).toBeInTheDocument();
    expect(view.container.querySelectorAll(".import-method-card")).toHaveLength(3);
    expect(view.container.querySelector(".import-method-grid")).toBeInTheDocument();
    expect(screen.getByText("添加第三方模型提供商")).toBeInTheDocument();
  });

  it("previews dropped JSON files without opening the picker", async () => {
    const preview: JsonProfileImportPreview = {
      preview_id: "drop-preview",
      expires_at_ms: 1_900_000_000_000,
      items: [
        {
          id: "drop-valid",
          file_name: "dropped.json",
          alias: "Dropped account",
          auth_mode: "oauth",
          source: "拖入文件",
          status: "valid",
          message: "已验证，可导入。",
          email: "drop@example.com",
          account_id: null,
          existing_profile_alias: null,
        },
      ],
    };
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    webview.onDragDropEvent.mockImplementation((handler) => {
      webview.dragHandler = handler as typeof webview.dragHandler;
      return Promise.resolve(webview.unlisten);
    });
    const previewImport = vi
      .spyOn(api, "previewJsonProfileImport")
      .mockResolvedValue(preview);

    renderProfiles();
    await waitFor(() => expect(webview.onDragDropEvent).toHaveBeenCalledOnce());
    await act(async () => {
      webview.dragHandler?.({
        payload: { type: "drop", paths: ["/tmp/dropped.json", "/tmp/readme.txt"] },
      });
    });

    await waitFor(() =>
      expect(previewImport).toHaveBeenCalledWith(["/tmp/dropped.json"]),
    );
    expect(vi.mocked(open)).not.toHaveBeenCalled();
    expect(
      await screen.findByRole("heading", { name: "选择要导入的账号" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: /Dropped account/ })).toBeChecked();
  });

  it("previews selected JSON files, defaults to verified accounts, and commits only checked items", async () => {
    const preview: JsonProfileImportPreview = {
      preview_id: "json-preview",
      expires_at_ms: 1_900_000_000_000,
      items: [
        {
          id: "valid",
          file_name: "auth.json",
          alias: "Verified OAuth",
          auth_mode: "oauth",
          source: "JSON 文件",
          status: "valid",
          message: "已验证，可导入。",
          email: "verified@example.com",
          account_id: null,
          existing_profile_alias: null,
        },
        {
          id: "invalid",
          file_name: "invalid.json",
          alias: "Rejected token",
          auth_mode: "oauth",
          source: "JSON 文件",
          status: "invalid",
          message: "凭据已被上游拒绝。",
          email: null,
          account_id: null,
          existing_profile_alias: null,
        },
      ],
    };
    vi.mocked(open).mockResolvedValue(["/tmp/auth.json", "/tmp/invalid.json"]);
    const previewImport = vi
      .spyOn(api, "previewJsonProfileImport")
      .mockResolvedValue(preview);
    const commitImport = vi.spyOn(api, "commitJsonProfileImport").mockResolvedValue({
      created: 1,
      updated: 0,
      skipped: 1,
      failed: 0,
      items: [
        {
          id: "valid",
          alias: "Verified OAuth",
          action: "created",
          message: "档案已创建。",
          profile_id: "profile-valid",
          auth_mode: "oauth",
        },
      ],
    });
    const syncImported = vi
      .spyOn(api, "syncProfileAccountInfo")
      .mockResolvedValue(
        profileFixture({ id: "profile-valid", alias: "Verified OAuth" }),
      );
    const refreshImportedModels = vi
      .spyOn(api, "refreshProfileModels")
      .mockResolvedValue({
        ...profileFixture({ id: "profile-valid", alias: "Verified OAuth" }),
        models: ["gpt-5"],
      });
    const onJsonImportComplete = vi.fn().mockResolvedValue(undefined);
    render(
      <Profiles
        profiles={[]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
        onJsonImportComplete={onJsonImportComplete}
      />,
    );

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);
    fireEvent.click(screen.getByRole("button", { name: "选择 JSON 文件" }));

    await waitFor(() =>
      expect(previewImport).toHaveBeenCalledWith([
        "/tmp/auth.json",
        "/tmp/invalid.json",
      ]),
    );
    expect(
      screen.getByRole("heading", { name: "选择要导入的账号" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: /Verified OAuth/ })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: /Rejected token/ })).toBeDisabled();

    fireEvent.click(screen.getByRole("button", { name: "导入 1 个账号" }));
    await waitFor(() =>
      expect(commitImport).toHaveBeenCalledWith("json-preview", ["valid"]),
    );
    expect(syncImported).toHaveBeenCalledWith("profile-valid");
    expect(refreshImportedModels).toHaveBeenCalledWith("profile-valid");
    expect(onJsonImportComplete).toHaveBeenCalledOnce();
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "选择要导入的账号" }),
      ).not.toBeInTheDocument(),
    );
  });

  it("retries unverified JSON preview items without reopening the file chooser", async () => {
    const preview: JsonProfileImportPreview = {
      preview_id: "retry-preview",
      expires_at_ms: 1_900_000_000_000,
      items: [
        {
          id: "retry",
          file_name: "refresh.json",
          alias: "Retry account",
          auth_mode: "oauth",
          source: "JSON 文件",
          status: "unverified",
          message: "暂时无法联网验证，请稍后重试预检。",
          email: null,
          account_id: null,
          existing_profile_alias: null,
        },
      ],
    };
    vi.mocked(open).mockResolvedValue(["/tmp/refresh.json"]);
    vi.spyOn(api, "previewJsonProfileImport").mockResolvedValue(preview);
    const retryImport = vi.spyOn(api, "retryJsonProfileImport").mockResolvedValue({
      ...preview,
      items: [{ ...preview.items[0], status: "valid", message: "已验证，可导入。" }],
    });
    renderProfiles();

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);
    fireEvent.click(screen.getByRole("button", { name: "选择 JSON 文件" }));
    await screen.findByRole("button", { name: "重试未验证项" });
    fireEvent.click(screen.getByRole("button", { name: "重试未验证项" }));

    await waitFor(() => expect(retryImport).toHaveBeenCalledWith("retry-preview"));
    expect(vi.mocked(open)).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("checkbox", { name: /Retry account/ })).toBeChecked();
    expect(screen.getByRole("button", { name: "导入 1 个账号" })).toBeEnabled();
  });

  it("explains that OAuth opens in the default browser", () => {
    renderProfiles();

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);

    expect(screen.getByText(/默认浏览器完成官方 OAuth 授权/)).toBeInTheDocument();
  });

  it("opens the same picker from the empty state", () => {
    renderProfiles();

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[1]);

    expect(screen.getByRole("heading", { name: "选择导入方式" })).toBeInTheDocument();
  });

  it("tests and saves a third-party provider with editable identity mappings", async () => {
    const onCreateApiProfile = vi.fn().mockResolvedValue(undefined);
    vi.spyOn(api, "testApiServiceProfile").mockResolvedValue({
      status: "verified",
      message: "已发现 2 个模型。",
      endpoint: "https://api.example.test/v1/models",
      latency_ms: 24,
      http_status: 200,
      category: "ok",
      model_count: 2,
      models: ["third-party-coder", "second-party-coder"],
    });
    vi.spyOn(api, "codexGatewayConfigStatus").mockResolvedValue({
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
      oauth_profile_options: [
        {
          id: "oauth-work",
          alias: "Work Login",
          available: true,
          reason: null,
        },
      ],
      history_sync: null,
    });
    render(
      <Profiles
        profiles={[]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onCreateApiProfile={onCreateApiProfile}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);
    fireEvent.click(screen.getByRole("button", { name: "添加提供商" }));
    expect(
      screen.getByRole("dialog", { name: "添加第三方模型提供商" }),
    ).toBeInTheDocument();
    const providerDialog = screen.getByRole("dialog", {
      name: "添加第三方模型提供商",
    });
    const dialogContent = providerDialog.querySelector(
      ".dialog-content",
    ) as HTMLElement;
    const sheet = dialogContent.querySelector(".profile-dialog-sheet") as HTMLElement;
    expect(dialogContent).toContainElement(sheet);
    const form = within(sheet);
    expect(form.getByText(/OpenAI 兼容 Relay/)).toBeInTheDocument();
    expect(
      form.getByText(/模型映射会生成 Codex model_catalog_json/),
    ).toBeInTheDocument();
    expect(
      form.getByText(/所有模型请求仍发送到当前第三方模型供应商服务/),
    ).toBeInTheDocument();
    const oauthSelector = form.getByRole("combobox", {
      name: "OAuth 登录档案（可选）",
    });
    expect(oauthSelector).toHaveTextContent("不绑定登录档案");
    fireEvent.click(oauthSelector);
    const oauthOption = await screen.findByRole("option", { name: /Work Login/ });
    expect(providerDialog.querySelector(".dialog-portal-root")).toContainElement(
      oauthOption,
    );
    fireEvent.click(oauthOption);
    expect(oauthSelector).toHaveTextContent("Work Login");
    fireEvent.change(form.getByLabelText("档案名称"), {
      target: { value: "Third Party" },
    });
    fireEvent.change(form.getByLabelText("Base URL"), {
      target: { value: "https://api.example.test/v1" },
    });
    fireEvent.change(form.getByLabelText("API Key"), {
      target: { value: "sk-test" },
    });
    fireEvent.click(form.getByRole("button", { name: "测试连接并发现模型" }));

    expect(await screen.findByText("连接已验证")).toBeInTheDocument();
    expect(
      form.queryByRole("group", { name: "发现的上游模型" }),
    ).not.toBeInTheDocument();
    expect(
      form.getByRole("button", { name: /发现的上游模型，已选择 0\/2/ }),
    ).toHaveTextContent("选择需要映射的模型");
    fireEvent.click(form.getByRole("button", { name: /发现的上游模型，已选择 0\/2/ }));
    fireEvent.click(form.getByRole("checkbox", { name: "third-party-coder" }));
    expect(
      form.getByRole("button", { name: /发现的上游模型，已选择 1\/2/ }),
    ).toHaveTextContent("third-party-coder");
    expect(form.getByLabelText("Model ID")).toHaveValue("third-party-coder");
    fireEvent.change(form.getByLabelText("Model ID"), {
      target: { value: "codex-visible-coder" },
    });
    fireEvent.click(form.getByRole("button", { name: "测试并保存" }));

    await waitFor(() =>
      expect(onCreateApiProfile).toHaveBeenCalledWith(
        expect.objectContaining({
          alias: "Third Party",
          provider: "openai_compatible",
          wire_api: "responses",
          base_url: "https://api.example.test/v1",
          api_key: "sk-test",
          codex_oauth_profile_id: "oauth-work",
          max_concurrency: 4,
          max_queue_depth: 8,
          queue_timeout_ms: 15_000,
          model_mappings: [
            {
              model: "codex-visible-coder",
              upstream_model: "third-party-coder",
              display_name: "third-party-coder",
              context_window: null,
            },
          ],
        }),
      ),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "添加第三方模型提供商" }),
      ).not.toBeInTheDocument(),
    );
  });

  it("clears the OAuth account when editing a third-party provider", async () => {
    const onUpdateApiProfile = vi.fn().mockResolvedValue(undefined);
    vi.spyOn(api, "codexGatewayConfigStatus").mockResolvedValue({
      enabled: true,
      mode: "third_party",
      config_path: "/Users/test/.codex/config.toml",
      service_url: "https://api.example.test/v1",
      message: "Codex 正在直连第三方模型提供商：Third Party。",
      auth_status: "ok",
      needs_repair: false,
      direct_profile_id: "api-active",
      direct_profile_alias: "Third Party",
      oauth_profile_id: "oauth-a",
      oauth_profile_alias: "Work A",
      oauth_profile_available: true,
      oauth_profile_options: [
        {
          id: "oauth-a",
          alias: "Work A",
          available: true,
          reason: null,
        },
        {
          id: "oauth-b",
          alias: "Work B",
          available: true,
          reason: null,
        },
      ],
      history_sync: null,
    });
    render(
      <Profiles
        profiles={[
          profileFixture({ id: "oauth-a", alias: "Work A" }),
          profileFixture({ id: "oauth-b", alias: "Work B" }),
          {
            id: "api-active",
            alias: "Third Party",
            kind: "api_key",
            base_url: "https://api.example.test/v1",
            provider: "openai_compatible",
            wire_api: "responses",
            enabled: true,
            in_pool: false,
            priority: 0,
            weight: 1,
            models: ["codex-visible"],
            model_mappings: [
              {
                model: "codex-visible",
                upstream_model: "provider-real",
                display_name: "Provider Real",
                context_window: null,
              },
            ],
            codex_oauth_profile_id: "oauth-a",
            health: "healthy",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
            validation_status: "unknown",
            validated_at_ms: null,
            validation_message: null,
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onUpdateApiProfile={onUpdateApiProfile}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "编辑 API 服务：Third Party" }));
    expect(
      screen.getByRole("dialog", { name: "编辑第三方模型提供商" }),
    ).toBeInTheDocument();
    const sheet = screen
      .getByRole("heading", { name: "编辑第三方模型提供商" })
      .closest("section") as HTMLElement;
    const form = within(sheet);
    const oauthSelector = form.getByRole("combobox", {
      name: "OAuth 登录档案（可选）",
    });
    await waitFor(() => expect(oauthSelector).toHaveTextContent("Work A"));
    fireEvent.click(oauthSelector);
    fireEvent.click(await screen.findByRole("option", { name: /不绑定登录档案/ }));
    fireEvent.click(form.getByRole("button", { name: "保存修改" }));

    await waitFor(() =>
      expect(onUpdateApiProfile).toHaveBeenCalledWith(
        expect.objectContaining({
          id: "api-active",
          api_key: null,
          codex_oauth_profile_id: null,
          provider: "openai_compatible",
          wire_api: "responses",
          base_url: "https://api.example.test/v1",
          model_mappings: [
            {
              model: "codex-visible",
              upstream_model: "provider-real",
              display_name: "Provider Real",
              context_window: null,
            },
          ],
        }),
      ),
    );
  });

  it("keeps the API edit dialog and values after a save failure", async () => {
    const onUpdateApiProfile = vi.fn().mockRejectedValue(new Error("数据库写入失败"));
    render(
      <Profiles
        profiles={[
          {
            ...profileFixture({
              id: "api-failure",
              alias: "Original Provider",
              kind: "api_key",
            }),
            base_url: "https://api.example.test/v1",
            provider: "openai_compatible",
            wire_api: "responses",
            models: ["provider-model"],
            model_mappings: [
              {
                model: "provider-model",
                upstream_model: "provider-model",
                display_name: "Provider Model",
                context_window: null,
              },
            ],
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onUpdateApiProfile={onUpdateApiProfile}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "编辑 API 服务：Original Provider" }),
    );
    const dialog = screen.getByRole("dialog", {
      name: "编辑第三方模型提供商",
    });
    fireEvent.change(within(dialog).getByLabelText("档案名称"), {
      target: { value: "Retained Provider" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "保存修改" }));

    expect(await within(dialog).findByText("数据库写入失败")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("档案名称")).toHaveValue("Retained Provider");
    expect(dialog).toBeInTheDocument();
  });

  it("blocks closing an API dialog while the save is pending", async () => {
    const pendingUpdate = deferred<void>();
    render(
      <Profiles
        profiles={[
          {
            ...profileFixture({
              id: "api-pending",
              alias: "Pending Provider",
              kind: "api_key",
            }),
            base_url: "https://api.example.test/v1",
            provider: "openai_compatible",
            wire_api: "responses",
            models: ["provider-model"],
            model_mappings: [
              {
                model: "provider-model",
                upstream_model: "provider-model",
                display_name: "Provider Model",
                context_window: null,
              },
            ],
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onUpdateApiProfile={() => pendingUpdate.promise}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "编辑 API 服务：Pending Provider" }),
    );
    const dialog = screen.getByRole("dialog", {
      name: "编辑第三方模型提供商",
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "保存修改" }));

    expect(
      await within(dialog).findByRole("button", { name: "正在保存提供商" }),
    ).toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "取消" })).toBeDisabled();
    fireEvent(dialog, new Event("cancel", { cancelable: true }));
    fireEvent.click(dialog);
    expect(dialog).toBeInTheDocument();

    pendingUpdate.resolve(undefined);
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "编辑第三方模型提供商" }),
      ).not.toBeInTheDocument(),
    );
  });

  it("resets model mappings from discovered upstream models", async () => {
    vi.spyOn(api, "testApiServiceProfile").mockResolvedValue({
      status: "verified",
      message: "已发现 2 个模型。",
      endpoint: "https://api.example.test/v1/models",
      latency_ms: 24,
      http_status: 200,
      category: "ok",
      model_count: 2,
      models: ["first-upstream", "second-upstream"],
    });
    render(
      <Profiles
        profiles={[]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onCreateApiProfile={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);
    fireEvent.click(screen.getByRole("button", { name: "添加提供商" }));
    const sheet = screen
      .getByRole("heading", { name: "添加第三方模型提供商" })
      .closest("section") as HTMLElement;
    const form = within(sheet);
    fireEvent.change(form.getByLabelText("Base URL"), {
      target: { value: "https://api.example.test/v1" },
    });
    fireEvent.change(form.getByLabelText("API Key"), {
      target: { value: "sk-test" },
    });
    fireEvent.click(form.getByRole("button", { name: "测试连接并发现模型" }));

    expect(await screen.findByText("连接已验证")).toBeInTheDocument();
    fireEvent.click(form.getByRole("button", { name: /发现的上游模型，已选择 0\/2/ }));
    fireEvent.click(form.getByRole("checkbox", { name: "first-upstream" }));
    fireEvent.change(form.getByLabelText("Model ID"), {
      target: { value: "codex-visible" },
    });

    fireEvent.click(form.getByRole("button", { name: "重置映射" }));

    const modelInputs = form.getAllByLabelText("Model ID");
    const upstreamInputs = form.getAllByLabelText("Upstream Model");
    expect(modelInputs).toHaveLength(2);
    expect(modelInputs[0]).toHaveValue("first-upstream");
    expect(modelInputs[1]).toHaveValue("second-upstream");
    expect(upstreamInputs[0]).toHaveValue("first-upstream");
    expect(upstreamInputs[1]).toHaveValue("second-upstream");
  });

  it("filters and bulk manages discovered upstream models from the dropdown", async () => {
    const longModel = "gpt-5.3-codex-spark-preview-super-long-provider-model-name";
    vi.spyOn(api, "testApiServiceProfile").mockResolvedValue({
      status: "verified",
      message: "已发现 5 个模型。",
      endpoint: "https://api.example.test/v1/models",
      latency_ms: 24,
      http_status: 200,
      category: "ok",
      model_count: 5,
      models: ["alpha-model", "", "beta-model", "alpha-model", longModel],
    });
    render(
      <Profiles
        profiles={[]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onCreateApiProfile={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);
    fireEvent.click(screen.getByRole("button", { name: "添加提供商" }));
    const sheet = screen
      .getByRole("heading", { name: "添加第三方模型提供商" })
      .closest("section") as HTMLElement;
    const form = within(sheet);
    fireEvent.change(form.getByLabelText("Base URL"), {
      target: { value: "https://api.example.test/v1" },
    });
    fireEvent.change(form.getByLabelText("API Key"), {
      target: { value: "sk-test" },
    });
    fireEvent.click(form.getByRole("button", { name: "测试连接并发现模型" }));

    expect(await screen.findByText("连接已验证")).toBeInTheDocument();
    const discoveryTrigger = form.getByRole("button", {
      name: /发现的上游模型，已选择 0\/3/,
    });
    fireEvent.click(discoveryTrigger);
    expect(discoveryTrigger.closest(".model-discovery-select")).toHaveClass("is-open");
    const longModelOption = form
      .getByRole("checkbox", { name: longModel })
      .closest(".model-discovery-select-option") as HTMLElement;
    expect(longModelOption).toHaveAttribute("title", longModel);
    expect(longModelOption).toHaveAttribute("data-selected", "false");
    expect(
      longModelOption.querySelector(".model-discovery-select-option-copy"),
    ).toBeInTheDocument();

    fireEvent.change(form.getByLabelText("搜索上游模型"), {
      target: { value: "spark" },
    });
    expect(
      form.queryByRole("checkbox", { name: "alpha-model" }),
    ).not.toBeInTheDocument();
    expect(form.getByRole("checkbox", { name: longModel })).toBeInTheDocument();

    fireEvent.change(form.getByLabelText("搜索上游模型"), {
      target: { value: "missing" },
    });
    expect(form.getByText("没有匹配的模型。")).toBeInTheDocument();

    fireEvent.change(form.getByLabelText("搜索上游模型"), {
      target: { value: "" },
    });
    fireEvent.click(form.getByRole("button", { name: "全选" }));
    expect(
      form
        .getByRole("checkbox", { name: longModel })
        .closest(".model-discovery-select-option"),
    ).toHaveAttribute("data-selected", "true");
    const modelInputs = form.getAllByLabelText("Model ID");
    expect(modelInputs).toHaveLength(3);
    expect(modelInputs[0]).toHaveValue("alpha-model");
    expect(modelInputs[1]).toHaveValue("beta-model");
    expect(modelInputs[2]).toHaveValue(longModel);
    expect(
      form.getByRole("button", { name: /发现的上游模型，已选择 3\/3/ }),
    ).toBeInTheDocument();

    fireEvent.click(form.getByRole("button", { name: "清空" }));
    expect(form.queryByLabelText("Model ID")).not.toBeInTheDocument();
    expect(
      form.getByRole("button", { name: /发现的上游模型，已选择 0\/3/ }),
    ).toBeInTheDocument();
  });

  it("shows the structured provider test failure report instead of a generic internal error", async () => {
    vi.spyOn(api, "testApiServiceProfile").mockResolvedValue({
      status: "failed",
      message: "上游返回的模型目录不是有效 JSON。",
      endpoint: "https://api.example.test/v1/models",
      latency_ms: 18,
      http_status: 200,
      category: "json",
      model_count: 0,
      models: [],
    });
    render(
      <Profiles
        profiles={[]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onCreateApiProfile={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);
    fireEvent.click(screen.getByRole("button", { name: "添加提供商" }));
    const sheet = screen
      .getByRole("heading", { name: "添加第三方模型提供商" })
      .closest("section") as HTMLElement;
    const form = within(sheet);
    fireEvent.change(form.getByLabelText("Base URL"), {
      target: { value: "https://api.example.test/v1" },
    });
    fireEvent.change(form.getByLabelText("API Key"), {
      target: { value: "sk-test" },
    });
    fireEvent.click(form.getByRole("button", { name: "测试连接并发现模型" }));

    expect(await screen.findByText("连接未验证")).toBeInTheDocument();
    expect(screen.getByText("上游返回的模型目录不是有效 JSON。")).toBeInTheDocument();
    expect(screen.queryByText(/internal/)).not.toBeInTheDocument();
  });

  it("shows API Key profiles with a dedicated Codex activation action", () => {
    render(
      <Profiles
        profiles={[
          profileFixture({ id: "oauth-work", alias: "Work Login" }),
          {
            id: "api-profile",
            alias: "Gateway only",
            kind: "api_key",
            base_url: "https://relay.example.com/v1",
            enabled: true,
            in_pool: true,
            priority: 0,
            weight: 1,
            models: ["gpt-5-codex"],
            codex_oauth_profile_id: "oauth-work",
            health: "healthy",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
            validation_status: "unknown",
            validated_at_ms: null,
            validation_message: null,
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    );

    expect(
      screen.queryByRole("button", { name: "设为当前档案：Gateway only" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "编辑 API 服务：Gateway only" }),
    ).toBeEnabled();
    expect(
      screen.getByRole("button", { name: "切换到 Codex：Gateway only" }),
    ).toBeEnabled();
    expect(screen.getByText("OAuth 登录档案：Work Login")).toBeInTheDocument();
  });

  it("disables Codex direct activation for providers that need Relay routing", () => {
    const onActivateApiProfile = vi.fn().mockResolvedValue(undefined);
    render(
      <Profiles
        profiles={[
          {
            id: "api-profile",
            alias: "Chat Provider",
            kind: "api_key",
            base_url: "https://relay.example.com/v1",
            provider: "openai_compatible",
            wire_api: "chat_completions",
            enabled: true,
            in_pool: true,
            priority: 0,
            weight: 1,
            models: ["chat-model"],
            codex_oauth_profile_id: null,
            health: "healthy",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
            validation_status: "unknown",
            validated_at_ms: null,
            validation_message: null,
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onActivateApiProfile={onActivateApiProfile}
        onDelete={vi.fn()}
      />,
    );

    const button = screen.getByRole("button", {
      name: "切换到 Codex：Chat Provider",
    });
    expect(button).toBeDisabled();
    expect(button).toHaveAttribute(
      "title",
      "该供应商需要 Relay 本地路由或协议适配，不能直连 Codex",
    );
    expect(button).toHaveTextContent("切换到 Codex 直连");
    fireEvent.click(button);
    expect(onActivateApiProfile).not.toHaveBeenCalled();
  });

  it("keeps API Key Codex activation available before pool join or model refresh", () => {
    const onActivateApiProfile = vi.fn().mockResolvedValue(undefined);
    render(
      <Profiles
        profiles={[
          {
            id: "api-profile",
            alias: "Needs setup",
            kind: "api_key",
            base_url: "https://relay.example.com/v1",
            enabled: true,
            in_pool: false,
            priority: 0,
            weight: 1,
            models: [],
            codex_oauth_profile_id: null,
            health: "unhealthy",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
            validation_status: "unknown",
            validated_at_ms: null,
            validation_message: null,
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onActivateApiProfile={onActivateApiProfile}
        onDelete={vi.fn()}
      />,
    );

    const button = screen.getByRole("button", { name: "切换到 Codex：Needs setup" });
    expect(button).toBeEnabled();
    expect(button).toHaveTextContent("测试并直连");
    fireEvent.click(button);
    expect(onActivateApiProfile).toHaveBeenCalledWith(
      expect.objectContaining({ id: "api-profile" }),
    );
  });

  it("keeps unrelated profile actions enabled while one profile joins the pool", async () => {
    const pendingToggle = deferred<void>();
    const onTogglePool = vi.fn().mockReturnValue(pendingToggle.promise);
    render(
      <Profiles
        profiles={[
          {
            ...profileFixture({ id: "oauth-profile", alias: "Personal OAuth" }),
            models: ["gpt-5"],
            health: "healthy",
          },
          {
            ...profileFixture({
              id: "api-profile",
              alias: "Verified API",
              kind: "api_key",
            }),
            base_url: "https://api.example.com/v1",
            models: ["gpt-5"],
            health: "healthy",
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onTogglePool={onTogglePool}
        onDelete={vi.fn()}
      />,
    );

    expect(
      screen.queryByText("当前没有可加入网关的 OAuth 或 API Key 档案。"),
    ).not.toBeInTheDocument();
    fireEvent.click(
      screen.getByRole("button", { name: "加入网关账号池：Personal OAuth" }),
    );
    expect(screen.queryByText(/本机.*运行时/)).not.toBeInTheDocument();
    expect(onTogglePool).toHaveBeenCalledWith(
      expect.objectContaining({ id: "oauth-profile" }),
    );
    expect(
      screen.getByRole("button", { name: "加入网关账号池：Verified API" }),
    ).toBeEnabled();
    expect(screen.getByText("正在加入")).toBeInTheDocument();

    pendingToggle.resolve(undefined);
    await waitFor(() => expect(screen.queryByText("正在加入")).not.toBeInTheDocument());
  });

  it("shows a bottom gateway join button even before OAuth models are refreshed", () => {
    const onTogglePool = vi.fn().mockResolvedValue(undefined);
    render(
      <Profiles
        profiles={[profileFixture({ id: "oauth-profile", alias: "Personal OAuth" })]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onTogglePool={onTogglePool}
        onDelete={vi.fn()}
      />,
    );

    const join = screen.getByRole("button", {
      name: "加入网关账号池：Personal OAuth",
    });
    expect(join).toBeEnabled();
    fireEvent.click(join);
    expect(onTogglePool).toHaveBeenCalledWith(
      expect.objectContaining({ id: "oauth-profile" }),
    );
  });

  it("shows a visible remove action for profiles already in the gateway pool", () => {
    const onTogglePool = vi.fn().mockResolvedValue(undefined);
    render(
      <Profiles
        profiles={[
          {
            ...profileFixture({ id: "oauth-profile", alias: "Personal OAuth" }),
            in_pool: true,
            models: ["gpt-5"],
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onTogglePool={onTogglePool}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "移出网关账号池：Personal OAuth" }),
    );
    expect(onTogglePool).toHaveBeenCalledWith(
      expect.objectContaining({ id: "oauth-profile" }),
    );
  });

  it("allows the current OAuth profile to be reapplied after a desktop restart failure", () => {
    const onSelect = vi.fn().mockResolvedValue(undefined);
    render(
      <Profiles
        profiles={[
          {
            id: "current-oauth",
            alias: "Current OAuth",
            kind: "codex_oauth",
            base_url: null,
            enabled: true,
            in_pool: false,
            priority: 0,
            weight: 1,
            models: [],
            codex_oauth_profile_id: null,
            health: "unknown",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: true,
            validation_status: "unknown",
            validated_at_ms: null,
            validation_message: null,
          },
        ]}
        busy={false}
        onSelect={onSelect}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    );

    const retry = screen.getByRole("button", { name: "设为当前档案：Current OAuth" });
    expect(retry).toBeEnabled();
    expect(retry).toHaveAttribute("title", "重新应用当前档案并复用原 Codex 客户端状态");
    fireEvent.click(retry);
    expect(onSelect).toHaveBeenCalledWith("current-oauth");
  });

  it("shows OAuth subscription and quota as concise account summaries", () => {
    const onSyncAccount = vi.fn().mockResolvedValue(undefined);
    const onStartOAuth = vi.fn().mockResolvedValue(status);
    const view = render(
      <Profiles
        profiles={[
          {
            id: "oauth-profile",
            alias: "Personal OAuth",
            kind: "codex_oauth",
            base_url: null,
            enabled: true,
            in_pool: false,
            priority: 0,
            weight: 1,
            models: [],
            codex_oauth_profile_id: null,
            health: "unknown",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
            validation_status: "unknown",
            validated_at_ms: null,
            validation_message: null,
            account: {
              display_name: "Ada Lovelace",
              email: "ada@example.com",
              account_id: "account_123",
              updated_at_ms: 1_735_689_600_000,
              quota: {
                status: "available",
                message: "已同步 Codex 额度。",
                source: "app_server",
                synced_at_ms: 1_900_000_000_000,
                last_attempt_at_ms: 1_735_689_600_000,
                last_error: null,
                primary: null,
                secondary: null,
                buckets: [
                  {
                    id: "codex",
                    name: "Codex",
                    plan_type: "pro",
                    primary: {
                      used_percent: 28,
                      remaining_percent: 72,
                      window_duration_mins: 300,
                      resets_at_ms: 1_900_010_800_000,
                    },
                    secondary: {
                      used_percent: 83,
                      remaining_percent: 17,
                      window_duration_mins: 10_080,
                      resets_at_ms: 1_900_604_800_000,
                    },
                  },
                  {
                    id: "extra",
                    name: "Extra",
                    plan_type: "pro",
                    primary: {
                      used_percent: 55,
                      remaining_percent: 45,
                      window_duration_mins: 60,
                      resets_at_ms: 1_900_003_600_000,
                    },
                    secondary: null,
                  },
                ],
                rate_limit_reached_type: null,
              },
              subscription: {
                status: "available",
                plan_type: "pro",
                period_ends_at_ms: 1_900_864_000_000,
                will_renew: true,
                source: "app_server+account_check",
                synced_at_ms: 1_735_689_600_000,
                last_attempt_at_ms: 1_735_689_600_000,
                last_error: null,
              },
            },
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={onStartOAuth}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={onSyncAccount}
        onDelete={vi.fn()}
      />,
    );

    expect(screen.getAllByText("Personal OAuth").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("ada@example.com")).toBeInTheDocument();
    expect(screen.queryByText("Ada Lovelace")).not.toBeInTheDocument();
    expect(screen.queryByText("account_123")).not.toBeInTheDocument();
    expect(
      view.container.querySelector(".profile-account-summary"),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("受管 OAuth")).not.toBeInTheDocument();
    expect(screen.queryByText("未加入账号池")).not.toBeInTheDocument();
    expect(screen.getByText("ChatGPT Pro")).toBeInTheDocument();
    expect(screen.getByText("短期")).toBeInTheDocument();
    expect(screen.getByText("长期")).toBeInTheDocument();
    expect(
      screen.getByRole("progressbar", { name: "短期额度剩余 72%" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("progressbar", { name: "长期额度剩余 17%" }),
    ).toBeInTheDocument();
    expect(screen.getByText("另有 1 个额度窗口")).toBeInTheDocument();
    expect(
      view.container.querySelector(".quota-meter-grid.is-single"),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("状态")).not.toBeInTheDocument();
    expect(screen.queryByText("模型")).not.toBeInTheDocument();
    expect(screen.getByText("优先级 / 权重")).toBeInTheDocument();
    expect(
      screen.queryByText(/优先通过本机 Codex 运行时读取套餐和额度/),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("Codex 运行时")).not.toBeInTheDocument();
    expect(onSyncAccount).not.toHaveBeenCalled();

    fireEvent.click(within(view.container).getByRole("button", { name: "刷新资料" }));
    expect(onSyncAccount).toHaveBeenCalledWith("oauth-profile");
    fireEvent.click(
      within(view.container).getByRole("button", { name: "更新凭据：Personal OAuth" }),
    );
    expect(onStartOAuth).toHaveBeenCalledWith("oauth-profile");
  });

  it("shows quota only when account data exists and keeps unsynced profiles compact", () => {
    const view = renderProfiles([
      profileFixture({
        id: "pat-profile",
        alias: "Personal PAT",
        authMode: "personal_access_token",
        account: accountSummary({ email: "pat@example.com", planType: "pro" }),
      }),
      profileFixture({
        id: "agent-profile",
        alias: "Build Agent",
        authMode: "agent_identity",
      }),
    ]);

    expect(screen.getByText("pat@example.com")).toBeInTheDocument();
    expect(screen.queryByText("个人访问令牌")).not.toBeInTheDocument();
    expect(screen.queryByText("Agent Identity")).not.toBeInTheDocument();
    expect(screen.getByText("ChatGPT Pro")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "刷新资料" })).toHaveLength(1);
    expect(screen.getByRole("button", { name: "同步资料" })).toBeInTheDocument();
    expect(view.container.querySelectorAll(".profile-card")).toHaveLength(2);
  });

  it("keeps unavailable dates and stale quota as compact statuses", () => {
    const view = render(
      <Profiles
        profiles={[
          {
            id: "stale-oauth",
            alias: "Stale OAuth",
            kind: "codex_oauth",
            base_url: null,
            enabled: true,
            in_pool: false,
            priority: 0,
            weight: 1,
            models: [],
            codex_oauth_profile_id: null,
            health: "unknown",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
            validation_status: "unknown",
            validated_at_ms: null,
            validation_message: null,
            account: {
              display_name: null,
              email: "stale@example.com",
              account_id: "hidden-account-id",
              updated_at_ms: 1_735_689_600_000,
              quota: {
                status: "stale",
                message: "缓存数据已过期。",
                source: "app_server",
                synced_at_ms: 1_735_689_600_000,
                last_attempt_at_ms: 1_735_689_600_000,
                last_error: "网络不可用",
                primary: {
                  used_percent: 60,
                  remaining_percent: 40,
                  window_duration_mins: 300,
                  resets_at_ms: null,
                },
                secondary: null,
                buckets: [],
                rate_limit_reached_type: null,
              },
              subscription: {
                status: "stale",
                plan_type: "plus",
                period_ends_at_ms: null,
                will_renew: null,
                source: "account_check",
                synced_at_ms: 1_735_689_600_000,
                last_attempt_at_ms: 1_735_689_600_000,
                last_error: "网络不可用",
              },
            },
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    );

    const card = within(view.container);
    expect(card.getAllByText("缓存已过期")).not.toHaveLength(0);
    expect(card.getByText("重置时间未提供")).toBeInTheDocument();
    expect(
      view.container.querySelector(".quota-meter-grid.is-single"),
    ).toBeInTheDocument();
    expect(card.queryByText("网络不可用")).not.toBeInTheDocument();
    expect(card.queryByText("hidden-account-id")).not.toBeInTheDocument();
  });

  it("makes keychain authorization a concise, recoverable sync state", () => {
    const onSyncAccount = vi.fn().mockResolvedValue(undefined);
    const view = render(
      <Profiles
        profiles={[
          {
            id: "locked-oauth",
            alias: "Locked OAuth",
            kind: "codex_oauth",
            base_url: null,
            enabled: true,
            in_pool: false,
            priority: 0,
            weight: 1,
            models: [],
            codex_oauth_profile_id: null,
            health: "unknown",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
            validation_status: "unknown",
            validated_at_ms: null,
            validation_message: null,
            account: {
              display_name: null,
              email: "locked@example.com",
              account_id: null,
              updated_at_ms: 1_735_689_600_000,
              quota: {
                status: "unavailable",
                message: "后台同步未读取钥匙串；点击“同步资料”后可在系统弹窗中授权。",
                source: null,
                synced_at_ms: null,
                last_attempt_at_ms: 1_735_689_600_000,
                last_error:
                  "后台同步未读取钥匙串；点击“同步资料”后可在系统弹窗中授权。",
                primary: null,
                secondary: null,
                buckets: [],
                rate_limit_reached_type: null,
              },
              subscription: {
                status: "unavailable",
                plan_type: null,
                period_ends_at_ms: null,
                will_renew: null,
                source: null,
                synced_at_ms: null,
                last_attempt_at_ms: 1_735_689_600_000,
                last_error: null,
              },
            },
          },
        ]}
        busy={false}
        onSelect={vi.fn().mockResolvedValue(undefined)}
        onStartOAuth={vi.fn().mockResolvedValue(status)}
        onOAuthStatus={vi.fn().mockResolvedValue(status)}
        onCancelOAuth={vi.fn().mockResolvedValue(undefined)}
        onCompleteOAuth={vi.fn().mockResolvedValue(undefined)}
        onSyncAccount={onSyncAccount}
        onDelete={vi.fn()}
      />,
    );

    expect(screen.getAllByText("需要授权")).not.toHaveLength(0);
    expect(screen.getByText("需要系统授权后刷新")).toBeInTheDocument();
    expect(
      screen.queryByText("后台同步未读取钥匙串；点击“同步资料”后可在系统弹窗中授权。"),
    ).not.toBeInTheDocument();
    fireEvent.click(within(view.container).getByRole("button", { name: "刷新资料" }));
    expect(onSyncAccount).toHaveBeenCalledWith("locked-oauth");
  });

  it("filters profiles by name and email together", () => {
    renderProfiles([
      profileFixture({
        id: "ada-personal",
        alias: "Ada Personal",
        account: accountSummary({ email: "ada@personal.example", planType: "plus" }),
      }),
      profileFixture({
        id: "ada-work",
        alias: "Ada Work",
        account: accountSummary({ email: "ada@company.example", planType: "pro" }),
      }),
      profileFixture({
        id: "grace",
        alias: "Grace Hopper",
        account: accountSummary({ email: "grace@company.example", planType: "pro" }),
      }),
    ]);

    fireEvent.change(screen.getByLabelText("档案名称"), { target: { value: "ada" } });
    expect(profileAliases()).toEqual(["Ada Personal", "Ada Work"]);

    fireEvent.change(screen.getByLabelText("邮箱"), {
      target: { value: "COMPANY.EXAMPLE" },
    });
    expect(profileAliases()).toEqual(["Ada Work"]);
    expect(screen.getByText("显示 1 / 共 3 个档案")).toBeInTheDocument();
  });

  it("filters dynamic subscription types and keeps unsynced filtering to OAuth profiles", () => {
    renderProfiles([
      profileFixture({
        id: "plus",
        alias: "Plus account",
        account: accountSummary({ email: "plus@example.com", planType: "plus" }),
      }),
      profileFixture({
        id: "pro",
        alias: "Pro account",
        account: accountSummary({ email: "pro@example.com", planType: "pro" }),
      }),
      profileFixture({
        id: "unsynced",
        alias: "Unsynced OAuth",
        account: accountSummary({ email: "pending@example.com", planType: null }),
      }),
      profileFixture({ id: "api", alias: "Gateway API", kind: "api_key" }),
    ]);

    fireEvent.click(screen.getByRole("button", { name: "订阅类型：全部套餐" }));
    const subscriptionMenu = screen.getByRole("listbox", { name: "订阅类型选项" });
    expect(
      within(subscriptionMenu).getByRole("option", { name: "ChatGPT Plus" }),
    ).toBeInTheDocument();
    expect(
      within(subscriptionMenu).getByRole("option", { name: "ChatGPT Pro" }),
    ).toBeInTheDocument();
    fireEvent.click(
      within(subscriptionMenu).getByRole("option", { name: "ChatGPT Pro" }),
    );
    expect(profileAliases()).toEqual(["Pro account"]);

    chooseMenuOption("订阅类型", "ChatGPT Pro", "尚未同步");
    expect(profileAliases()).toEqual(["Unsynced OAuth"]);
  });

  it("sorts by the most urgent quota window, subscription time, and reset time", () => {
    renderProfiles([
      profileFixture({
        id: "late",
        alias: "Late",
        account: accountSummary({
          email: "late@example.com",
          planType: "plus",
          periodEndsAtMs: 500,
          windows: [quotaWindow(80, 500)],
        }),
      }),
      profileFixture({
        id: "balanced",
        alias: "Balanced",
        account: accountSummary({
          email: "balanced@example.com",
          planType: "plus",
          periodEndsAtMs: 300,
          windows: [quotaWindow(30, 300)],
        }),
      }),
      profileFixture({
        id: "same",
        alias: "Same value",
        account: accountSummary({
          email: "same@example.com",
          planType: "plus",
          periodEndsAtMs: 400,
          windows: [quotaWindow(30, 300)],
        }),
      }),
      profileFixture({
        id: "urgent",
        alias: "Urgent",
        account: accountSummary({
          email: "urgent@example.com",
          planType: "pro",
          periodEndsAtMs: 200,
          windows: [quotaWindow(60, 900), quotaWindow(5, 100), quotaWindow(40, 700)],
        }),
      }),
      profileFixture({ id: "unknown", alias: "Unknown" }),
    ]);

    chooseMenuOption("排序方式", "默认顺序", "剩余额度");
    expect(profileAliases()).toEqual([
      "Urgent",
      "Balanced",
      "Same value",
      "Late",
      "Unknown",
    ]);

    fireEvent.click(screen.getByRole("button", { name: "当前紧急优先，点击切换" }));
    expect(profileAliases()).toEqual([
      "Late",
      "Balanced",
      "Same value",
      "Urgent",
      "Unknown",
    ]);

    chooseMenuOption("排序方式", "剩余额度", "订阅剩余时间");
    expect(profileAliases()).toEqual([
      "Urgent",
      "Balanced",
      "Same value",
      "Late",
      "Unknown",
    ]);

    chooseMenuOption("排序方式", "订阅剩余时间", "额度重置时间");
    expect(profileAliases()).toEqual([
      "Urgent",
      "Balanced",
      "Same value",
      "Late",
      "Unknown",
    ]);
  });

  it("shows a no-results state and clears all active filtering controls", () => {
    renderProfiles([
      profileFixture({
        id: "one",
        alias: "One account",
        account: accountSummary({ email: "one@example.com", planType: "plus" }),
      }),
      profileFixture({
        id: "two",
        alias: "Two account",
        account: accountSummary({ email: "two@example.com", planType: "pro" }),
      }),
    ]);

    fireEvent.change(screen.getByLabelText("档案名称"), {
      target: { value: "missing" },
    });
    expect(screen.getByRole("heading", { name: "没有匹配的档案" })).toBeInTheDocument();
    expect(screen.getByText("显示 0 / 共 2 个档案")).toBeInTheDocument();

    fireEvent.click(screen.getAllByRole("button", { name: "清除筛选" })[0]);
    expect(profileAliases()).toEqual(["One account", "Two account"]);
    expect(screen.getByText("显示 2 / 共 2 个档案")).toBeInTheDocument();
  });

  it("opens only one filter menu at a time and restores focus after Escape", () => {
    renderProfiles([
      profileFixture({
        id: "pro",
        alias: "Pro account",
        account: accountSummary({ email: "pro@example.com", planType: "pro" }),
      }),
    ]);

    const subscriptionTrigger = screen.getByRole("button", {
      name: "订阅类型：全部套餐",
    });
    const sortTrigger = screen.getByRole("button", { name: "排序方式：默认顺序" });
    fireEvent.click(subscriptionTrigger);

    expect(subscriptionTrigger).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("option", { name: "全部套餐" })).toHaveAttribute(
      "aria-selected",
      "true",
    );

    fireEvent.click(sortTrigger);
    expect(subscriptionTrigger).toHaveAttribute("aria-expanded", "false");
    expect(sortTrigger).toHaveAttribute("aria-expanded", "true");

    fireEvent.keyDown(screen.getByRole("option", { name: "默认顺序" }), {
      key: "Escape",
    });
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(sortTrigger).toHaveFocus();
  });

  it("supports keyboard selection and closes menus on Tab or outside interaction", () => {
    renderProfiles([
      profileFixture({
        id: "plus",
        alias: "Plus account",
        account: accountSummary({ email: "plus@example.com", planType: "plus" }),
      }),
      profileFixture({
        id: "pro",
        alias: "Pro account",
        account: accountSummary({ email: "pro@example.com", planType: "pro" }),
      }),
    ]);

    const subscriptionTrigger = screen.getByRole("button", {
      name: "订阅类型：全部套餐",
    });
    subscriptionTrigger.focus();
    fireEvent.keyDown(subscriptionTrigger, { key: "ArrowDown" });

    const allOption = screen.getByRole("option", { name: "全部套餐" });
    expect(allOption).toHaveFocus();
    fireEvent.keyDown(allOption, { key: "ArrowDown" });
    const plusOption = screen.getByRole("option", { name: "ChatGPT Plus" });
    expect(plusOption).toHaveFocus();
    fireEvent.keyDown(plusOption, { key: "Enter" });

    expect(profileAliases()).toEqual(["Plus account"]);
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "订阅类型：ChatGPT Plus" }),
    ).toHaveFocus();

    const selectedTrigger = screen.getByRole("button", {
      name: "订阅类型：ChatGPT Plus",
    });
    fireEvent.click(selectedTrigger);
    fireEvent.keyDown(screen.getByRole("option", { name: "ChatGPT Plus" }), {
      key: "Tab",
    });
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();

    fireEvent.click(selectedTrigger);
    fireEvent.pointerDown(document.body);
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  });

  it("selects an option on pointer down before WebKit can reorder focus events", () => {
    renderProfiles([
      profileFixture({
        id: "plus",
        alias: "Plus account",
        account: accountSummary({ email: "plus@example.com", planType: "plus" }),
      }),
      profileFixture({
        id: "pro",
        alias: "Pro account",
        account: accountSummary({ email: "pro@example.com", planType: "pro" }),
      }),
    ]);

    fireEvent.click(screen.getByRole("button", { name: "订阅类型：全部套餐" }));
    const proOption = screen.getByRole("option", { name: "ChatGPT Pro" });

    fireEvent.pointerDown(proOption);

    expect(profileAliases()).toEqual(["Pro account"]);
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  });
  it("renders K-12 and suppresses stale unknown subscription labels", () => {
    renderProfiles([
      profileFixture({
        id: "k12",
        alias: "K12 account",
        account: accountSummary({ email: "k12@example.com", planType: "k12" }),
      }),
      profileFixture({
        id: "unknown",
        alias: "Unknown account",
        account: accountSummary({ email: "unknown@example.com", planType: "unknown" }),
      }),
    ]);

    expect(screen.getByText("ChatGPT K-12 / Edu")).toBeInTheDocument();
    expect(screen.getByText("套餐尚未同步")).toBeInTheDocument();
    expect(screen.queryByText(/未知套餐/)).not.toBeInTheDocument();
  });
});
