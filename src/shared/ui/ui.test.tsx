import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Button, Dialog, Select, StatusPill } from ".";

afterEach(cleanup);

describe("shared UI", () => {
  it("exposes stable button and status variants", () => {
    render(
      <>
        <Button variant="primary">保存</Button>
        <StatusPill tone="running">运行中</StatusPill>
      </>,
    );

    expect(screen.getByRole("button", { name: "保存" })).toHaveClass("button-primary");
    expect(screen.getByText("运行中")).toHaveClass("running");
  });

  it("closes a dialog from the native cancel event", () => {
    const onClose = vi.fn();
    render(<Dialog open title="确认操作" onClose={onClose} />);

    fireEvent(screen.getByRole("dialog"), new Event("cancel"));

    expect(onClose).toHaveBeenCalledOnce();
  });

  it("opens the shared select, exposes descriptions, and selects an option", () => {
    const onValueChange = vi.fn();
    render(
      <Select
        ariaLabel="目标档案"
        onValueChange={onValueChange}
        options={[
          { value: "one", label: "工作账号", description: "ChatGPT Pro" },
          { value: "two", label: "停用账号", disabled: true },
        ]}
        placeholder="选择档案"
        value=""
      />,
    );

    const trigger = screen.getByRole("combobox", { name: "目标档案" });
    expect(trigger).toHaveTextContent("选择档案");
    fireEvent.click(trigger);
    expect(screen.getByText("ChatGPT Pro")).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "停用账号" })).toHaveAttribute(
      "data-disabled",
    );
    fireEvent.click(screen.getByRole("option", { name: /工作账号/ }));
    expect(onValueChange).toHaveBeenCalledWith("one");
  });
});
