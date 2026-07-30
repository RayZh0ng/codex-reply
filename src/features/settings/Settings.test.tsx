import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { Settings } from "./Settings";

describe("Settings", () => {
  it("changes the desktop workspace mode and restores or removes fresh workspaces", () => {
    const changeMode = vi.fn().mockResolvedValue(undefined);
    const restore = vi.fn().mockResolvedValue(undefined);
    const remove = vi.fn();
    const changeTheme = vi.fn();
    const changeUpdateSettings = vi.fn().mockResolvedValue(undefined);
    const checkUpdate = vi.fn().mockResolvedValue(undefined);
    const installUpdate = vi.fn().mockResolvedValue(undefined);
    render(
      <Settings
        settings={{ mode: "per_profile" }}
        workspaces={[
          {
            id: "workspace-1",
            profile_id: "profile-1",
            profile_alias: "个人账号",
            created_at_ms: 1_700_000_000_000,
            last_launched_at_ms: 1_700_000_000_000,
          },
        ]}
        codexEnvironment={{
          platform: "windows",
          codex_home: "C:\\Users\\dev\\.codex",
          can_install: true,
          message: "检测到 1 个 Codex 环境项需要处理，可执行一键部署。",
          last_checked_at_ms: 1_700_000_000_000,
          summary: {
            status: "action_required",
            ok_count: 0,
            warning_count: 0,
            missing_count: 1,
            failed_count: 0,
            fixable_count: 1,
            health_percent: 0,
          },
          checks: [
            {
              id: "codex_cli",
              label: "Codex CLI",
              status: "missing",
              detail: "未在 PATH 中检测到 codex。",
              command: "npm.cmd install --global @openai/codex",
              description: "协作任务依赖 CLI。",
              next_action: "可执行修复命令。",
              automatic: true,
            },
          ],
          install_steps: [
            {
              id: "codex_cli",
              label: "安装 Codex CLI",
              available: true,
              command: "npm.cmd install --global @openai/codex",
              requires_privilege: false,
              next_action: "安装完成后重新检查。",
            },
          ],
          manual_commands: ["npm.cmd install --global @openai/codex"],
        }}
        codexEnvironmentInstall={null}
        codexEnvironmentBusy={false}
        busy={false}
        themePreference="system"
        updateSettings={{ channel: "stable", auto_check: true }}
        availableUpdate={{
          version: "0.2.0-beta.1",
          current_version: "0.1.0",
          body: "测试更新说明",
          date: null,
          channel: "beta",
        }}
        updateStatus={null}
        updateBusy={false}
        onChangeMode={changeMode}
        onThemePreferenceChange={changeTheme}
        onChangeUpdateSettings={changeUpdateSettings}
        onCheckUpdate={checkUpdate}
        onInstallUpdate={installUpdate}
        onRefreshCodexEnvironment={vi.fn().mockResolvedValue(undefined)}
        onInstallCodexEnvironment={vi.fn().mockResolvedValue(undefined)}
        onRestore={restore}
        onDelete={remove}
      />,
    );

    fireEvent.click(screen.getByRole("radio", { name: /共享原客户端状态/ }));
    expect(changeMode).toHaveBeenCalledWith("shared");
    fireEvent.click(screen.getByRole("radio", { name: "深色" }));
    expect(changeTheme).toHaveBeenCalledWith("dark");
    fireEvent.click(screen.getByRole("radio", { name: /Beta 版/ }));
    expect(changeUpdateSettings).toHaveBeenCalledWith({
      channel: "beta",
      auto_check: true,
    });
    fireEvent.click(screen.getByRole("checkbox", { name: "启动时自动检查更新" }));
    expect(changeUpdateSettings).toHaveBeenCalledWith({
      channel: "stable",
      auto_check: false,
    });
    expect(screen.getByText(/发现\s*Beta\s*更新：0.2.0-beta.1/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "立即检查更新" }));
    expect(checkUpdate).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "安装并重启" }));
    expect(installUpdate).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "恢复" }));
    expect(restore).toHaveBeenCalledWith("workspace-1");
    fireEvent.click(screen.getByRole("button", { name: "删除 个人账号 的工作区" }));
    expect(remove).toHaveBeenCalledWith("workspace-1", "个人账号");
  });

  it("renders cross-platform environment health, warnings, failures and install logs", () => {
    render(
      <Settings
        settings={{ mode: "shared" }}
        workspaces={[]}
        codexEnvironment={{
          platform: "linux",
          codex_home: "/home/dev/.codex",
          can_install: false,
          message: "Linux 检测到 2 个提示项，请查看检查卡片中的原因和下一步。",
          last_checked_at_ms: 1_700_000_000_000,
          summary: {
            status: "warning",
            ok_count: 1,
            warning_count: 1,
            missing_count: 0,
            failed_count: 1,
            fixable_count: 0,
            health_percent: 53,
          },
          checks: [
            {
              id: "node",
              label: "Node.js LTS",
              status: "ok",
              detail: "v24.16.0",
              command: null,
              description: "Codex CLI 依赖 Node.js。",
              next_action: "无需处理。",
              automatic: false,
            },
            {
              id: "relay_ca",
              label: "Relay CA 信任",
              status: "warning",
              detail: "Relay CA 尚未通过系统信任校验。",
              command:
                "sudo cp /tmp/gateway-ca.pem /usr/local/share/ca-certificates/codex-relay-gateway-ca.crt && sudo update-ca-certificates",
              description:
                "第三方 provider 切换到 Codex 时需要系统信任 Relay HTTPS CA。",
              next_action: "可在网关页执行信任 CA。",
              automatic: false,
            },
            {
              id: "codex_home",
              label: "Codex home",
              status: "failed",
              detail: "/home/dev/.codex 不可写。",
              command: null,
              description: "保存 Codex config.toml。",
              next_action: "请修复目录权限后重新检查。",
              automatic: false,
            },
          ],
          install_steps: [],
          manual_commands: [
            "sudo cp /tmp/gateway-ca.pem /usr/local/share/ca-certificates/codex-relay-gateway-ca.crt && sudo update-ca-certificates",
          ],
        }}
        codexEnvironmentInstall={{
          status: "failed",
          message: "部分 Codex 环境部署步骤需要继续处理。",
          logs: [
            {
              step_id: "node",
              label: "安装 Node.js LTS",
              status: "needs_privilege",
              detail: "permission denied",
              command: "pkexec sh -lc 'apt-get install -y nodejs npm'",
              next_action: "按系统授权弹窗完成安装。",
            },
          ],
          environment: {
            platform: "linux",
            codex_home: "/home/dev/.codex",
            can_install: false,
            message: "Linux 检测到提示项。",
            last_checked_at_ms: 1_700_000_000_000,
            summary: {
              status: "warning",
              ok_count: 1,
              warning_count: 1,
              missing_count: 0,
              failed_count: 1,
              fixable_count: 0,
              health_percent: 53,
            },
            checks: [],
            install_steps: [],
            manual_commands: [],
          },
        }}
        codexEnvironmentBusy={false}
        busy={false}
        themePreference="system"
        updateSettings={{ channel: "stable", auto_check: false }}
        availableUpdate={null}
        updateStatus={null}
        updateBusy={false}
        onChangeMode={vi.fn().mockResolvedValue(undefined)}
        onThemePreferenceChange={vi.fn()}
        onChangeUpdateSettings={vi.fn().mockResolvedValue(undefined)}
        onCheckUpdate={vi.fn().mockResolvedValue(undefined)}
        onInstallUpdate={vi.fn().mockResolvedValue(undefined)}
        onRefreshCodexEnvironment={vi.fn().mockResolvedValue(undefined)}
        onInstallCodexEnvironment={vi.fn().mockResolvedValue(undefined)}
        onRestore={vi.fn().mockResolvedValue(undefined)}
        onDelete={vi.fn()}
      />,
    );

    expect(screen.getByText("53")).toBeInTheDocument();
    expect(screen.getByText(/平台：Linux/)).toBeInTheDocument();
    expect(screen.getByText("Relay CA 信任")).toBeInTheDocument();
    expect(screen.getByText("失败")).toBeInTheDocument();
    expect(screen.getByText(/permission denied/)).toBeInTheDocument();
    const deployButtons = screen.getAllByRole("button", { name: "一键部署缺失项" });
    const copyButtons = screen.getAllByRole("button", { name: "复制修复命令" });
    expect(deployButtons[deployButtons.length - 1]).toBeDisabled();
    expect(copyButtons[copyButtons.length - 1]).toBeEnabled();
  });
});
