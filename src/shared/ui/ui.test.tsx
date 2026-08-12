import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  Button,
  Dialog,
  EmptyState,
  InlineNotice,
  PageHeader,
  Select,
  StatusPill,
} from ".";

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

  it("locks a busy dialog and exposes button progress", () => {
    const onClose = vi.fn();
    render(
      <Dialog busy open title="正在保存" onClose={onClose}>
        <Button loading loadingLabel="正在保存" variant="primary">
          保存
        </Button>
      </Dialog>,
    );

    const dialog = screen.getByRole("dialog", { name: "正在保存" });
    fireEvent(dialog, new Event("cancel"));
    fireEvent.click(dialog);

    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "正在保存" })).toBeDisabled();
  });

  it("renders shared page, empty, and notice patterns", () => {
    render(
      <>
        <PageHeader description="统一页面说明" eyebrow="Workspace" title="工作台" />
        <EmptyState title="暂无任务" description="创建任务后会显示在这里。" />
        <InlineNotice compact role="note" tone="warning" title="离线状态">
          本机核心暂不可用。
        </InlineNotice>
      </>,
    );

    expect(screen.getByRole("heading", { name: "工作台" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "暂无任务" })).toBeInTheDocument();
    expect(screen.getByRole("note")).toHaveClass("inline-notice-compact");
    expect(screen.getByRole("note")).toHaveTextContent("本机核心暂不可用");
  });

  it("portals a select inside its nearest dialog and selects an option", () => {
    const onValueChange = vi.fn();
    render(
      <Dialog open title="编辑档案">
        <Select
          ariaLabel="目标档案"
          onValueChange={onValueChange}
          options={[
            { value: "one", label: "工作账号" },
            { value: "two", label: "个人账号" },
          ]}
          value="one"
        />
      </Dialog>,
    );

    const dialog = screen.getByRole("dialog", { name: "编辑档案" });
    fireEvent.click(screen.getByRole("combobox", { name: "目标档案" }));
    const listbox = screen.getByRole("listbox");
    const option = screen.getByRole("option", { name: "个人账号" });
    const portalRoot = dialog.querySelector(".dialog-portal-root");

    expect(portalRoot).toContainElement(listbox);
    expect(portalRoot).toContainElement(option);
    expect(dialog.querySelector(".dialog-content")).not.toContainElement(listbox);
    fireEvent.click(option);
    expect(onValueChange).toHaveBeenCalledWith("two");
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
    const standaloneListbox = screen.getByRole("listbox");
    expect(
      standaloneListbox.closest("[data-radix-popper-content-wrapper]")?.parentElement,
    ).toBe(document.body);
    expect(standaloneListbox.closest("dialog")).toBeNull();
    expect(screen.getByText("ChatGPT Pro")).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "停用账号" })).toHaveAttribute(
      "data-disabled",
    );
    fireEvent.click(screen.getByRole("option", { name: /工作账号/ }));
    expect(onValueChange).toHaveBeenCalledWith("one");
  });
});
