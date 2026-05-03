/**
 * Cortex Protocol discovery for the TypeScript SDK.
 *
 * Mirrors the Python `thinkingroot.cortex` and the Rust
 * `thinkingroot_core::cortex` reader so JS/TS consumers can attach to
 * an already-running `root serve` daemon without spawning a duplicate
 * process.  Reads the lockfile at `<config_dir>/thinkingroot/
 * cortex.lock`; supports schema_version <= SUPPORTED_SCHEMA per the
 * reader-bumped versioning contract.
 *
 * Spec: `docs/2026-05-02-unified-singleton-runtime.md` §3.4.
 */

import { readFile } from "node:fs/promises";
import { homedir, platform } from "node:os";
import { join } from "node:path";

import { CortexError, IncompatibleLockSchema } from "./errors.js";

export const SUPPORTED_SCHEMA = 1;
export const LIVENESS_PATH = "/livez";
export const DEFAULT_HOST = "127.0.0.1";
export const DEFAULT_PORT = 31760;

export interface CortexLock {
  schema_version: number;
  pid: number;
  port: number;
  host: string;
  version: string;
  started_by: string;
  started_at: string;
  binary_path: string;
}

/**
 * Resolve the canonical lockfile path for the current OS.  Honors
 * `XDG_CONFIG_HOME` on Linux for parity with the Rust `dirs::
 * config_dir()` resolution.
 */
export function lockPath(): string {
  const plat = platform();
  let base: string;
  if (plat === "darwin") {
    base = join(homedir(), "Library", "Application Support");
  } else if (plat === "win32") {
    const appdata = process.env["APPDATA"];
    base = appdata ?? join(homedir(), "AppData", "Roaming");
  } else {
    const xdg = process.env["XDG_CONFIG_HOME"];
    base = xdg ?? join(homedir(), ".config");
  }
  return join(base, "thinkingroot", "cortex.lock");
}

/**
 * Read and parse the cortex lockfile, or `null` when absent.
 *
 * @throws {CortexError} when the file is unreadable or malformed.
 * @throws {IncompatibleLockSchema} when `schema_version > SUPPORTED_SCHEMA`.
 */
export async function readLock(): Promise<CortexLock | null> {
  const path = lockPath();
  let raw: string;
  try {
    raw = await readFile(path, "utf-8");
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code === "ENOENT") return null;
    throw new CortexError(`read ${path}: ${(err as Error).message}`);
  }

  let data: Record<string, unknown>;
  try {
    data = JSON.parse(raw) as Record<string, unknown>;
  } catch (err) {
    throw new CortexError(
      `parse ${path}: ${(err as Error).message}`,
    );
  }

  const schema = Number(data["schema_version"] ?? 0);
  if (schema > SUPPORTED_SCHEMA) {
    throw new IncompatibleLockSchema(
      `cortex.lock schema_version=${schema} exceeds supported ${SUPPORTED_SCHEMA} ` +
        `— upgrade the thinkingroot npm package`,
    );
  }

  return {
    schema_version: schema,
    pid: Number(data["pid"]),
    port: Number(data["port"]),
    host: String(data["host"] ?? DEFAULT_HOST),
    version: String(data["version"] ?? ""),
    started_by: String(data["started_by"] ?? ""),
    started_at: String(data["started_at"] ?? ""),
    binary_path: String(data["binary_path"] ?? ""),
  };
}

/**
 * Cross-platform PID liveness check.  On POSIX uses `process.kill(pid, 0)`;
 * on Windows uses the same call which is implemented by Node via
 * `OpenProcess` + `GetExitCodeProcess`.  Returns `false` for any
 * lookup failure (including permission errors, which the Rust side
 * tolerantly treats as "alive but not ours" — we follow the same
 * convention so `connect()` doesn't false-negative on locked-down
 * systems).
 */
export function processAlive(pid: number): boolean {
  if (pid <= 0) return false;
  try {
    // Signal 0 doesn't deliver — it just probes whether the PID is
    // valid + we have permission to signal it.
    process.kill(pid, 0);
    return true;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    // EPERM means the process exists but we don't own it.  Treat as
    // alive — matches Rust's `sysinfo` + Python's `os.kill` behaviour.
    return code === "EPERM";
  }
}

/**
 * 1s GET `<host>:<port>/livez` health check.  Returns `true` on a
 * 2xx response; any error (timeout, refused, non-2xx) returns `false`.
 */
export async function healthCheck(
  host: string,
  port: number,
): Promise<boolean> {
  const url = `http://${host}:${port}${LIVENESS_PATH}`;
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), 1000);
  try {
    const resp = await fetch(url, { signal: ctrl.signal });
    return resp.ok;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}
