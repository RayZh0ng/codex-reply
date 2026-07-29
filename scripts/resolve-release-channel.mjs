const stablePattern = /^v(\d+)\.(\d+)\.(\d+)$/;
const betaPattern = /^v(\d+)\.(\d+)\.(\d+)-beta\.(\d+)$/;

export function resolveReleaseChannel(refName) {
  if (stablePattern.test(refName)) {
    return {
      version: refName.slice(1),
      channel: "stable",
      prerelease: false,
      manifestName: "stable.json",
    };
  }
  if (betaPattern.test(refName)) {
    return {
      version: refName.slice(1),
      channel: "beta",
      prerelease: true,
      manifestName: "beta.json",
    };
  }
  throw new Error(`Unsupported release tag "${refName}". Use v1.2.3 or v1.2.3-beta.1.`);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const refName = process.argv[2] ?? process.env.GITHUB_REF_NAME;
  if (!refName) {
    console.error(
      "Missing release tag. Pass the tag as argv[2] or set GITHUB_REF_NAME.",
    );
    process.exit(1);
  }
  try {
    const release = resolveReleaseChannel(refName);
    console.log(`version=${release.version}`);
    console.log(`channel=${release.channel}`);
    console.log(`prerelease=${release.prerelease}`);
    console.log(`manifest_name=${release.manifestName}`);
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(1);
  }
}
