import { describe, expect, it } from "vitest";

import { resolveReleaseChannel } from "./resolve-release-channel.mjs";

describe("resolveReleaseChannel", () => {
  it("classifies stable tags", () => {
    expect(resolveReleaseChannel("v1.2.3")).toEqual({
      version: "1.2.3",
      channel: "stable",
      prerelease: false,
      manifestName: "stable.json",
    });
  });

  it("classifies beta prerelease tags", () => {
    expect(resolveReleaseChannel("v1.2.3-beta.4")).toEqual({
      version: "1.2.3-beta.4",
      channel: "beta",
      prerelease: true,
      manifestName: "beta.json",
    });
  });

  it("rejects unsupported release tags", () => {
    expect(() => resolveReleaseChannel("v1.2.3-alpha.1")).toThrow(
      /Unsupported release tag/,
    );
  });
});
