import { setTheme as setNativeTheme } from "@tauri-apps/api/app";
import { useCallback, useEffect, useMemo, useState } from "react";

export type ThemePreference = "system" | "light" | "dark";
export type ResolvedTheme = Exclude<ThemePreference, "system">;

const THEME_STORAGE_KEY = "codex-relay.theme.v1";
const DARK_THEME_QUERY = "(prefers-color-scheme: dark)";

export function readThemePreference(): ThemePreference {
  try {
    const value = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (value === "light" || value === "dark" || value === "system") return value;
  } catch {
    // Storage can be unavailable in hardened WebViews; system is the stable fallback.
  }
  return "system";
}

export function resolveTheme(
  preference: ThemePreference,
  systemPrefersDark = window.matchMedia(DARK_THEME_QUERY).matches,
): ResolvedTheme {
  if (preference === "system") return systemPrefersDark ? "dark" : "light";
  return preference;
}

export function applyTheme(
  preference: ThemePreference,
  systemPrefersDark = window.matchMedia(DARK_THEME_QUERY).matches,
) {
  const resolved = resolveTheme(preference, systemPrefersDark);
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
  return resolved;
}

export function initialiseTheme() {
  return applyTheme(readThemePreference());
}

export function useTheme() {
  const [preference, setPreferenceState] =
    useState<ThemePreference>(readThemePreference);
  const [systemPrefersDark, setSystemPrefersDark] = useState(
    () => window.matchMedia(DARK_THEME_QUERY).matches,
  );
  const resolvedTheme = useMemo(
    () => resolveTheme(preference, systemPrefersDark),
    [preference, systemPrefersDark],
  );

  useEffect(() => {
    const media = window.matchMedia(DARK_THEME_QUERY);
    const update = (event: MediaQueryListEvent | MediaQueryList) =>
      setSystemPrefersDark(event.matches);
    update(media);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);

  useEffect(() => {
    applyTheme(preference, systemPrefersDark);
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, preference);
    } catch {
      // The applied in-memory preference remains valid for this session.
    }
  }, [preference, systemPrefersDark]);

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    void setNativeTheme(preference === "system" ? null : preference).catch(() => {
      // CSS remains authoritative if a platform does not expose native theme control.
    });
  }, [preference]);

  const setPreference = useCallback((next: ThemePreference) => {
    setPreferenceState(next);
  }, []);

  return { preference, resolvedTheme, setPreference } as const;
}
