# Security Policy

## Supported versions

Codex Relay is currently published as beta software. Security fixes target the latest beta release and the `dev` branch.

| Version          | Supported   |
| ---------------- | ----------- |
| `0.2.x-beta`     | ✅          |
| `< 0.2.0-beta.1` | best effort |

## Reporting a vulnerability

Please report vulnerabilities by opening a private security advisory on GitHub or by contacting the maintainers listed in the repository profile. Include:

- affected version or commit SHA;
- reproduction steps and relevant logs;
- whether credentials, local files, gateway endpoints, or collaboration callbacks are involved;
- expected impact and any suggested fix.

Do not include real OAuth tokens, API keys, client keys, bot secrets, or private chat content. Use redacted examples such as `TOKEN`, `CLIENT_KEY`, and `CHAT_ID`.

## Local secret handling

Codex Relay stores OAuth credentials, API keys, client keys, and collaboration bot secrets in the app data directory using the local encrypted vault. SQLite rows and IPC payloads keep only metadata, masks, or secret references. JSON import previews are held in memory for a short time and are not written to SQLite before explicit confirmation.

## Scope notes

The API gateway and collaboration callbacks are local-first features. When exposing them to a LAN, tunnel, or public callback URL, restrict CIDR allowlists, rotate client keys regularly, and keep bot/webhook secrets out of issue trackers and logs.
