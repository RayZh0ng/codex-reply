import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type {
  MaskedProfile,
  ProfileAccountSummary,
  ProfileQuotaWindow,
} from "../../shared/ipc";
import { Profiles } from "./Profiles";

const status = {
  attempt_id: "attempt-1",
  profile_id: null,
  phase: "authorizing" as const,
  message: "已在默认浏览器中打开 OpenAI / ChatGPT 登录页面。",
};

afterEach(cleanup);

function renderProfiles(profiles: MaskedProfile[] = []) {
  return render(
    <Profiles
      profiles={profiles}
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
}: {
  id: string;
  alias: string;
  account?: ProfileAccountSummary | null;
  kind?: MaskedProfile["kind"];
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
    health: "unknown",
    cooldown_until_ms: null,
    credential_configured: true,
    is_current: false,
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

describe("Profiles OAuth import", () => {
  it("states that profile switching uses saved credentials without reauthorization", () => {
    renderProfiles();

    expect(
      screen.getByText("当前档案会按所选模式启动 Codex 工作区"),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/添加账号时会保存 OAuth 凭据。当前模式为/),
    ).toBeInTheDocument();
    expect(screen.getByText(/不会迁移 ChatGPT Chat\/Work/)).toBeInTheDocument();
  });

  it("opens the OAuth-only import picker from the page action", () => {
    renderProfiles();

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);

    expect(screen.getByText("使用 OpenAI / ChatGPT 登录")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "继续使用 OAuth" })).toBeInTheDocument();
    expect(screen.queryByText("API Key 上游")).not.toBeInTheDocument();
  });

  it("explains that OAuth opens in the default browser", () => {
    renderProfiles();

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[0]);

    expect(screen.getByText(/默认浏览器中打开官方 OAuth 页面/)).toBeInTheDocument();
  });

  it("opens the same picker from the empty state", () => {
    renderProfiles();

    fireEvent.click(screen.getAllByRole("button", { name: "添加档案" })[1]);

    expect(screen.getByRole("heading", { name: "选择导入方式" })).toBeInTheDocument();
  });

  it("marks API Key profiles as unavailable for the managed Codex current profile", () => {
    render(
      <Profiles
        profiles={[
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
            health: "healthy",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
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
      screen.getByRole("button", { name: "设为当前档案：Gateway only" }),
    ).toBeDisabled();
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
            health: "unknown",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: true,
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
    expect(retry).toHaveAttribute(
      "title",
      "重新应用当前档案并启动独立 ChatGPT/Codex 工作区",
    );
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
            health: "unknown",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
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

    expect(screen.getByText("Personal OAuth")).toBeInTheDocument();
    expect(screen.getByText("ada@example.com")).toBeInTheDocument();
    expect(screen.queryByText("Ada Lovelace")).not.toBeInTheDocument();
    expect(screen.queryByText("account_123")).not.toBeInTheDocument();
    expect(screen.queryByText("资料更新时间")).not.toBeInTheDocument();
    expect(screen.queryByText("受管 OAuth")).not.toBeInTheDocument();
    expect(screen.queryByText("未加入账号池")).not.toBeInTheDocument();
    expect(screen.getByText("ChatGPT Pro")).toBeInTheDocument();
    expect(screen.getByText("自动续费")).toBeInTheDocument();
    expect(screen.getByText("距下次续费")).toBeInTheDocument();
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
    expect(screen.queryByText("优先级 / 权重")).not.toBeInTheDocument();
    expect(
      screen.queryByText(/优先通过本机 Codex 运行时读取套餐和额度/),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("Codex 运行时")).not.toBeInTheDocument();
    expect(onSyncAccount).not.toHaveBeenCalled();

    fireEvent.click(within(view.container).getByRole("button", { name: "同步资料" }));
    expect(onSyncAccount).toHaveBeenCalledWith("oauth-profile");
    fireEvent.click(
      within(view.container).getByRole("button", { name: "更新凭据：Personal OAuth" }),
    );
    expect(onStartOAuth).toHaveBeenCalledWith("oauth-profile");
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
            health: "unknown",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
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
    expect(card.getAllByText("缓存已过期")).toHaveLength(2);
    expect(card.getByText("上游未提供续费日期")).toBeInTheDocument();
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
            health: "unknown",
            cooldown_until_ms: null,
            credential_configured: true,
            is_current: false,
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

    expect(screen.getByText("需要授权")).toBeInTheDocument();
    expect(screen.getByText("需要系统授权后同步")).toBeInTheDocument();
    expect(
      screen.queryByText("后台同步未读取钥匙串；点击“同步资料”后可在系统弹窗中授权。"),
    ).not.toBeInTheDocument();
    fireEvent.click(within(view.container).getByRole("button", { name: "解锁并同步" }));
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
});
