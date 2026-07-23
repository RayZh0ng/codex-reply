import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { Settings } from "./Settings";

describe("Settings", () => {
  it("changes the desktop workspace mode and restores or removes fresh workspaces", () => {
    const changeMode = vi.fn().mockResolvedValue(undefined);
    const restore = vi.fn().mockResolvedValue(undefined);
    const remove = vi.fn();
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
        onChangeMode={changeMode}
        onRestore={restore}
        onDelete={remove}
      />,
    );

    fireEvent.click(screen.getByRole("radio", { name: /共享原客户端状态/ }));
    expect(changeMode).toHaveBeenCalledWith("shared");
    fireEvent.click(screen.getByRole("button", { name: "恢复" }));
    expect(restore).toHaveBeenCalledWith("workspace-1");
    fireEvent.click(screen.getByRole("button", { name: "删除 个人账号 的工作区" }));
    expect(remove).toHaveBeenCalledWith("workspace-1", "个人账号");
  });
});
