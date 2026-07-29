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
});
