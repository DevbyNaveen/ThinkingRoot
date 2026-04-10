# Security Policy

## Supported Versions

| Version | Supported |
|---|---|
| latest (main) | ✅ |
| older releases | ❌ — please upgrade |

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Report them privately via [GitHub Security Advisories](https://github.com/thinkingroot/thinkingroot/security/advisories/new).

Please include:
- A description of the vulnerability
- Steps to reproduce
- The potential impact
- Any suggested mitigations

We will respond within **72 hours** and aim to release a patch within **14 days** for critical issues.

## Threat Model

ThinkingRoot processes potentially untrusted content (external documents, web pages, git commits). Key security considerations:

- **Prompt injection** — LLM extraction is performed over user-supplied content. Claims extracted from untrusted sources receive `TrustLevel::Untrusted` or `TrustLevel::Quarantined`. Do not serve untrusted artifacts directly to end users without review.
- **Sensitive data leakage** — Claims can be labeled with `Sensitivity` levels (Public / Internal / Confidential / Restricted). The REST API does not currently enforce sensitivity filtering by caller role — do not expose the API publicly with sensitive workspaces unless access control is added.
- **Local filesystem access** — `root compile` reads files from the specified directory. Do not run it on directories with secrets unless `.thinkingroot/config.toml` excludes them via `parsers.exclude_patterns`.
- **API key storage** — API keys are passed via CLI flags or environment variables and are never stored in `config.toml`. Use `.env` (gitignored) or a secrets manager.

## Known Limitations

- The REST API provides no row-level access control — any caller with a valid API key can read all claims including those with high sensitivity labels. This is planned for Phase 3 (safety engine).
- MCP tools do not authenticate individual tool calls beyond the server-level API key.
