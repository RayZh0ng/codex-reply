#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";

const args = new Set(process.argv.slice(2));
const live = args.has("--live");
const home = process.env.HOME;
if (!home) throw new Error("HOME is required");

function commandSpec(version) {
  const envName =
    version === "0.144.6" ? "CODEX_COMPAT_BIN_0144" : "CODEX_COMPAT_BIN_0147";
  if (process.env[envName]) return { command: process.env[envName], prefix: [] };
  const cask = `/opt/homebrew/Caskroom/codex/${version}/codex-aarch64-apple-darwin`;
  if (existsSync(cask)) return { command: cask, prefix: [] };
  if (version === "0.144.6") {
    const backup = `${home}/.codex/backups/codex-0.144.6/codex-aarch64-apple-darwin`;
    if (existsSync(backup)) return { command: backup, prefix: [] };
    return { command: "codex", prefix: [] };
  }
  return { command: "npx", prefix: ["-y", `@openai/codex@${version}`] };
}

function run(spec, commandArgs, options = {}) {
  const result = spawnSync(spec.command, [...spec.prefix, ...commandArgs], {
    encoding: "utf8",
    timeout: options.timeout ?? 30_000,
    env: options.env ?? process.env,
    cwd: options.cwd,
    detached: options.detached ?? false,
  });
  return {
    ok: result.status === 0,
    status: result.status,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? "",
  };
}

function temporaryCodexHome() {
  const root = mkdtempSync(join(tmpdir(), "codex-relay-compat-"));
  const source = join(home, ".codex");
  for (const file of ["auth.json", "config.toml", "codex-relay-model-catalog.json"]) {
    const from = join(source, file);
    if (existsSync(from)) cpSync(from, join(root, file));
  }
  return root;
}

async function appServerProbe(spec, codexHome) {
  const child = spawn(
    spec.command,
    [...spec.prefix, "app-server", "--stdio", "--strict-config"],
    {
      env: { ...process.env, CODEX_HOME: codexHome },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  const requests = [
    {
      method: "initialize",
      id: 1,
      params: {
        clientInfo: {
          name: "codex-relay-compat",
          title: "Codex Relay Compat",
          version: "1",
        },
        capabilities: {},
      },
    },
    { method: "initialized", params: {} },
    { method: "account/read", id: 2, params: { refreshToken: false } },
    { method: "model/list", id: 3, params: {} },
    {
      method: "thread/list",
      id: 4,
      params: {
        sourceKinds: [
          "cli",
          "vscode",
          "exec",
          "appServer",
          "subAgent",
          "subAgentReview",
          "subAgentCompact",
          "subAgentThreadSpawn",
          "subAgentOther",
          "unknown",
        ],
      },
    },
  ];
  for (const request of requests) child.stdin.write(`${JSON.stringify(request)}\n`);
  const responses = new Map();
  let buffer = "";
  let stderr = "";
  child.stderr.on("data", (chunk) => {
    stderr += String(chunk);
  });
  child.stdout.on("data", (chunk) => {
    buffer += String(chunk);
    const lines = buffer.split("\n");
    buffer = lines.pop() ?? "";
    for (const line of lines) {
      try {
        const message = JSON.parse(line);
        if (message.id != null) responses.set(Number(message.id), message);
      } catch {}
    }
  });
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline && ![1, 2, 3, 4].every((id) => responses.has(id))) {
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  const threads =
    responses.get(4)?.result?.data ?? responses.get(4)?.result?.threads ?? [];
  const firstThreadId = Array.isArray(threads)
    ? (threads[0]?.id ?? threads[0]?.threadId)
    : null;
  if (firstThreadId) {
    child.stdin.write(
      `${JSON.stringify({ method: "thread/read", id: 5, params: { threadId: firstThreadId, includeTurns: true } })}\n`,
    );
    while (Date.now() < deadline && !responses.has(5)) {
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }
  child.kill("SIGTERM");
  return {
    initialize: responses.get(1)?.error == null,
    accountReadable: responses.get(2)?.error == null,
    accountPresent: Boolean(responses.get(2)?.result?.account),
    authMethod: responses.get(2)?.result?.authMethod ?? null,
    models: Array.isArray(responses.get(3)?.result?.data)
      ? responses.get(3).result.data.length
      : Array.isArray(responses.get(3)?.result?.models)
        ? responses.get(3).result.models.length
        : 0,
    threadList: responses.get(4)?.error == null,
    listedThreads: Array.isArray(threads) ? threads.length : 0,
    threadRead: firstThreadId ? responses.get(5)?.error == null : null,
    stderrRetry: stderr.includes("stream disconnected - retrying sampling request"),
  };
}

const requestedVersions = (process.env.CODEX_COMPAT_VERSIONS ?? "0.144.6,0.147.0")
  .split(",")
  .map((value) => value.trim())
  .filter(Boolean);
const results = [];
for (const expected of requestedVersions) {
  const spec = commandSpec(expected);
  const codexHome = temporaryCodexHome();
  const schemaDir = join(codexHome, "schema");
  const workspace = join(codexHome, "workspace");
  mkdirSync(schemaDir, { recursive: true });
  mkdirSync(workspace, { recursive: true });
  try {
    const env = { ...process.env, CODEX_HOME: codexHome };
    const version = run(spec, ["--version"], { env });
    const schema = run(
      spec,
      ["app-server", "generate-json-schema", "--out", schemaDir],
      { env, timeout: 60_000 },
    );
    let exec = { ok: true, stderrRetry: false, fileWritten: false };
    if (live) {
      const task = run(
        spec,
        [
          "exec",
          "--strict-config",
          "--ignore-user-config",
          "--ignore-rules",
          "--skip-git-repo-check",
          "--sandbox",
          "workspace-write",
          "--json",
          "--ephemeral",
          "-C",
          workspace,
          "Create a file named compat-ok.txt in the current directory with the exact contents ok, then reply exactly done.",
        ],
        { env, cwd: workspace, timeout: 120_000 },
      );
      exec = {
        ok: task.ok,
        stderrRetry: task.stderr.includes(
          "stream disconnected - retrying sampling request",
        ),
        fileWritten:
          existsSync(join(workspace, "compat-ok.txt")) &&
          readFileSync(join(workspace, "compat-ok.txt"), "utf8").trim() === "ok",
      };
    }
    const protocol = await appServerProbe(spec, codexHome);
    results.push({
      expected,
      binary: basename(spec.command),
      version: version.stdout.trim() || version.stderr.trim().split("\n").at(-1),
      versionOk: version.ok && (version.stdout + version.stderr).includes(expected),
      strictConfigAndSchema: schema.ok,
      protocol,
      exec,
    });
  } finally {
    rmSync(codexHome, { recursive: true, force: true });
  }
}

const failed = results.some(
  (result) =>
    !result.versionOk ||
    !result.strictConfigAndSchema ||
    !result.protocol.initialize ||
    !result.protocol.threadList ||
    !result.protocol.accountReadable ||
    result.protocol.models < 1 ||
    (live &&
      (!result.exec.ok ||
        !result.exec.fileWritten ||
        result.exec.stderrRetry ||
        result.protocol.listedThreads < 1 ||
        result.protocol.threadRead !== true)),
);
console.log(
  JSON.stringify({ generatedAt: new Date().toISOString(), live, results }, null, 2),
);
process.exitCode = failed ? 1 : 0;
