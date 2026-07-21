import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import App from "./App";

describe("App", () => {
  it("renders the starter application shell", () => {
    render(<App />);

    expect(
      screen.getByRole("heading", { name: "桌面应用脚手架已就绪" }),
    ).toBeInTheDocument();
    expect(
      screen.getByText("尚未启用业务命令、文件访问或外部网络权限。"),
    ).toBeInTheDocument();
  });
});
