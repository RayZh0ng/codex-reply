export type DesktopPlatform = "macos" | "windows" | "linux" | "other";

export function detectDesktopPlatform(userAgent: string): DesktopPlatform {
  const normalized = userAgent.toLowerCase();
  if (normalized.includes("macintosh") || normalized.includes("mac os")) {
    return "macos";
  }
  if (normalized.includes("windows")) return "windows";
  if (normalized.includes("linux")) return "linux";
  return "other";
}
