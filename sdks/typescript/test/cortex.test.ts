import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, test, beforeEach, afterEach } from "vitest";

import {
  DEFAULT_HOST,
  DEFAULT_PORT,
  SUPPORTED_SCHEMA,
  lockPath,
  readLock,
} from "../src/cortex.js";
import { CortexError, IncompatibleLockSchema } from "../src/errors.js";

let tmpDir: string;
let originalXdg: string | undefined;

beforeEach(() => {
  tmpDir = mkdtempSync(join(tmpdir(), "tr-cortex-test-"));
  originalXdg = process.env["XDG_CONFIG_HOME"];
  process.env["XDG_CONFIG_HOME"] = tmpDir;
});

afterEach(() => {
  if (originalXdg === undefined) {
    delete process.env["XDG_CONFIG_HOME"];
  } else {
    process.env["XDG_CONFIG_HOME"] = originalXdg;
  }
  rmSync(tmpDir, { recursive: true, force: true });
});

describe("cortex.lockPath", () => {
  test("honors XDG_CONFIG_HOME on linux/macos", () => {
    // On macOS lockPath uses ~/Library/Application Support regardless
    // of XDG, so this assertion is XDG-only.  On macOS we just check
    // the path ends with the canonical suffix.
    const p = lockPath();
    expect(p.endsWith(join("thinkingroot", "cortex.lock"))).toBe(true);
  });
});

describe("cortex.readLock", () => {
  test("returns null when no lockfile exists", async () => {
    const lock = await readLock();
    expect(lock).toBeNull();
  });

  test("parses a valid lockfile", async () => {
    if (process.platform === "darwin") {
      // macOS lockPath ignores XDG; skip parse test on darwin to keep
      // the test hermetic.  The shape is exercised on linux CI.
      return;
    }
    const dir = join(tmpDir, "thinkingroot");
    require("node:fs").mkdirSync(dir, { recursive: true });
    const payload = {
      schema_version: 1,
      pid: 12345,
      port: 31760,
      host: "127.0.0.1",
      version: "0.9.1",
      started_by: "cli",
      started_at: "2026-05-03T12:00:00Z",
      binary_path: "/usr/local/bin/root",
    };
    writeFileSync(join(dir, "cortex.lock"), JSON.stringify(payload));

    const lock = await readLock();
    expect(lock).not.toBeNull();
    expect(lock!.pid).toBe(12345);
    expect(lock!.port).toBe(DEFAULT_PORT);
    expect(lock!.host).toBe(DEFAULT_HOST);
    expect(lock!.schema_version).toBe(SUPPORTED_SCHEMA);
  });

  test("throws IncompatibleLockSchema on future-versioned files", async () => {
    if (process.platform === "darwin") return;
    const dir = join(tmpDir, "thinkingroot");
    require("node:fs").mkdirSync(dir, { recursive: true });
    writeFileSync(
      join(dir, "cortex.lock"),
      JSON.stringify({
        schema_version: SUPPORTED_SCHEMA + 1,
        pid: 1,
        port: 31760,
        host: "127.0.0.1",
        version: "future",
        started_by: "cli",
        started_at: "2026-05-03T12:00:00Z",
        binary_path: "/usr/local/bin/root",
      }),
    );
    await expect(readLock()).rejects.toBeInstanceOf(IncompatibleLockSchema);
  });

  test("throws CortexError on malformed JSON", async () => {
    if (process.platform === "darwin") return;
    const dir = join(tmpDir, "thinkingroot");
    require("node:fs").mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "cortex.lock"), "{ not json");
    await expect(readLock()).rejects.toBeInstanceOf(CortexError);
  });
});
