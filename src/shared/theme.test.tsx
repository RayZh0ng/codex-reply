import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { initialiseTheme, useTheme } from "./theme";

const nativeTheme = vi.hoisted(() => ({
  setTheme: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@tauri-apps/api/app", () => nativeTheme);

function ThemeProbe() {
  const { preference, resolvedTheme, setPreference } = useTheme();
  return (
    <div>
      <output>{`${preference}:${resolvedTheme}`}</output>
      <button type="button" onClick={() => setPreference("system")}>
        系统
      </button>
      <button type="button" onClick={() => setPreference("light")}>
        浅色
      </button>
      <button type="button" onClick={() => setPreference("dark")}>
        深色
      </button>
    </div>
  );
}

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  delete document.documentElement.dataset.theme;
  document.documentElement.style.colorScheme = "";
  delete (window as typeof window & { __TAURI_INTERNALS__?: unknown })
    .__TAURI_INTERNALS__;
  nativeTheme.setTheme.mockClear();
  vi.restoreAllMocks();
});

describe("theme preference", () => {
  it("applies a saved preference before render and syncs manual changes natively", async () => {
    window.localStorage.setItem("codex-relay.theme.v1", "dark");
    expect(initialiseTheme()).toBe("dark");
    expect(document.documentElement).toHaveAttribute("data-theme", "dark");

    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    render(<ThemeProbe />);
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));

    expect(screen.getByText("light:light")).toBeInTheDocument();
    expect(window.localStorage.getItem("codex-relay.theme.v1")).toBe("light");
    expect(document.documentElement).toHaveAttribute("data-theme", "light");
    await waitFor(() => expect(nativeTheme.setTheme).toHaveBeenLastCalledWith("light"));
  });

  it("tracks operating-system changes only while the system preference is active", () => {
    let dark = false;
    const listeners = new Set<(event: MediaQueryListEvent) => void>();
    vi.spyOn(window, "matchMedia").mockImplementation(
      (query) =>
        ({
          matches: query.includes("prefers-color-scheme") ? dark : false,
          media: query,
          onchange: null,
          addEventListener: (
            _type: string,
            listener: (event: MediaQueryListEvent) => void,
          ) => listeners.add(listener),
          removeEventListener: (
            _type: string,
            listener: (event: MediaQueryListEvent) => void,
          ) => listeners.delete(listener),
          addListener: () => undefined,
          removeListener: () => undefined,
          dispatchEvent: () => false,
        }) as MediaQueryList,
    );

    render(<ThemeProbe />);
    expect(screen.getByText("system:light")).toBeInTheDocument();

    act(() => {
      dark = true;
      for (const listener of listeners)
        listener({ matches: true } as MediaQueryListEvent);
    });
    expect(screen.getByText("system:dark")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    act(() => {
      dark = false;
      for (const listener of listeners)
        listener({ matches: false } as MediaQueryListEvent);
    });
    expect(screen.getByText("light:light")).toBeInTheDocument();
  });
});
