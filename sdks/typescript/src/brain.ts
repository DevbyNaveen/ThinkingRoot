/**
 * High-level Brain facade — the canonical entry point for the
 * TypeScript SDK.  Mirrors the Python `thinkingroot.Brain` shape so
 * polyglot teams move between languages without re-learning the API.
 *
 * Three constructors:
 *   - `Brain.remote(url)`  — explicit URL.
 *   - `Brain.connect()`    — cortex-aware auto-discovery.
 *   - `Brain.mount(path)`  — spawn `root mount`, attach to result.
 *
 * Pure-TS / pure-fetch — no native bindings, no runtime deps beyond
 * Node 18's built-in `fetch`.
 *
 * Spec: `docs/secondary-brain-concept.md` §4.
 */

import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";

import { Client, type ClientOptions } from "./client.js";
import { ApiError, ConnectionError } from "./errors.js";
import {
  DEFAULT_HOST,
  DEFAULT_PORT,
  processAlive,
  readLock,
} from "./cortex.js";
import type {
  Claim,
  Entity,
  EngramRef,
  EngramScope,
  HybridResponse,
  MaterializeResponse,
  MountSummary,
  ProbeAnswer,
  RetrievalRequest,
  SearchResult,
  WorkspaceInfo,
} from "./types.js";

export interface BrainOptions {
  /** Workspace name.  Defaults to the first workspace mounted on the daemon. */
  workspace?: string;
  /** Bring-your-own session id.  Defaults to a fresh `ts-<16 hex>` token. */
  sessionId?: string;
  /** API token for password-protected daemons. */
  apiKey?: string | null;
  /** Per-request timeout in milliseconds.  Default 120s. */
  timeoutMs?: number;
  /** Override the global `fetch` (testing seam). */
  fetch?: typeof fetch;
}

export interface BrainInfo {
  transport: "remote";
  workspace: string;
  baseUrl: string;
  sessionId: string;
  daemonPid: number | null;
  daemonStartedBy: string | null;
}

const SESSION_HEADER = "X-TR-Session-Id";

/**
 * Unified facade for talking to a ThinkingRoot daemon.
 *
 * Construct via the `remote`, `connect`, or `mount` static methods —
 * never the constructor directly.
 */
export class Brain {
  private readonly _client: Client;
  private readonly _workspace: string;
  private readonly _sessionId: string;
  private readonly _baseUrl: string;
  private _daemonPid: number | null = null;
  private _daemonStartedBy: string | null = null;

  private constructor(
    client: Client,
    workspace: string,
    sessionId: string,
    baseUrl: string,
  ) {
    this._client = client;
    this._workspace = workspace;
    this._sessionId = sessionId;
    this._baseUrl = baseUrl;
  }

  // ─── Constructors ──────────────────────────────────────────

  /**
   * Attach to a running daemon at `baseUrl`.  When `workspace` is
   * omitted, the first workspace mounted on the daemon is used.
   */
  static async remote(
    baseUrl: string = `http://${DEFAULT_HOST}:${DEFAULT_PORT}`,
    opts: BrainOptions = {},
  ): Promise<Brain> {
    const clientOpts: ClientOptions = {
      baseUrl,
      apiKey: opts.apiKey,
      timeoutMs: opts.timeoutMs,
      fetch: opts.fetch,
    };
    const client = new Client(clientOpts);
    const workspace = opts.workspace ?? (await resolveFirstWorkspace(client));
    const sessionId = opts.sessionId ?? newSessionId();
    return new Brain(client, workspace, sessionId, client.baseUrl);
  }

  /**
   * Cortex-aware auto-discovery.  Reads the cortex lockfile and
   * attaches to the daemon when one is alive.  Throws
   * {@link ConnectionError} otherwise — the pure-fetch SDK has no
   * in-process fallback (that's what the Rust + Python SDKs offer).
   */
  static async connect(opts: BrainOptions = {}): Promise<Brain> {
    const lock = await readLock();
    if (lock === null) {
      throw new ConnectionError(
        "no cortex daemon running. Start one with `root serve` and " +
          "retry, or use Brain.remote(url) with an explicit URL.",
      );
    }
    if (!processAlive(lock.pid)) {
      throw new ConnectionError(
        `cortex.lock points to pid ${lock.pid} but the process is dead. ` +
          "Run `root serve` to start a fresh daemon.",
      );
    }
    const baseUrl = `http://${lock.host}:${lock.port}`;
    const brain = await Brain.remote(baseUrl, opts);
    brain._daemonPid = lock.pid;
    brain._daemonStartedBy = lock.started_by;
    return brain;
  }

  /**
   * Mount a `.tr` pack via the `root mount` CLI subcommand and
   * return a Brain attached to the freshly-mounted workspace.
   *
   * Requires the `root` binary on `$PATH`.
   */
  static async mount(
    packPath: string,
    options: { name?: string; noVerify?: boolean; recompile?: boolean } = {},
  ): Promise<Brain> {
    const args = ["mount", packPath];
    if (options.name) args.push("--name", options.name);
    if (options.noVerify) args.push("--no-verify");
    if (options.recompile) args.push("--recompile");

    const summary = await runRootCli<MountSummary>(args);
    const restUrl = summary.rest_url.replace(/\/$/, "");
    const marker = "/api/v1/ws/";
    const baseUrl = restUrl.includes(marker)
      ? restUrl.split(marker)[0]!
      : restUrl;

    const brain = await Brain.remote(baseUrl, {
      workspace: summary.workspace,
    });
    brain._daemonPid = summary.daemon_pid;
    brain._daemonStartedBy = "root_mount";
    return brain;
  }

  // ─── Introspection ─────────────────────────────────────────

  get workspace(): string {
    return this._workspace;
  }

  get sessionId(): string {
    return this._sessionId;
  }

  info(): BrainInfo {
    return {
      transport: "remote",
      workspace: this._workspace,
      baseUrl: this._baseUrl,
      sessionId: this._sessionId,
      daemonPid: this._daemonPid,
      daemonStartedBy: this._daemonStartedBy,
    };
  }

  // ─── Workspace listing ─────────────────────────────────────

  workspaces(): Promise<WorkspaceInfo[]> {
    return this._client.get<WorkspaceInfo[]>("/workspaces");
  }

  // ─── Claims / Entities / Search ────────────────────────────

  entities(): Promise<Entity[]> {
    return this._client.get<Entity[]>(`/ws/${this._workspace}/entities`);
  }

  entity(name: string): Promise<Entity> {
    return this._client.get<Entity>(
      `/ws/${this._workspace}/entities/${encodeURIComponent(name)}`,
    );
  }

  claims(opts: {
    type?: string;
    minConfidence?: number;
    limit?: number;
  } = {}): Promise<Claim[]> {
    const params = new URLSearchParams();
    if (opts.type) params.set("type", opts.type);
    if (opts.minConfidence !== undefined)
      params.set("min_confidence", String(opts.minConfidence));
    if (opts.limit !== undefined) params.set("limit", String(opts.limit));
    const qs = params.toString();
    const path = `/ws/${this._workspace}/claims${qs ? `?${qs}` : ""}`;
    return this._client.get<Claim[]>(path);
  }

  search(query: string, topK: number = 10): Promise<SearchResult> {
    const params = new URLSearchParams({ q: query, top_k: String(topK) });
    return this._client.get<SearchResult>(
      `/ws/${this._workspace}/search?${params.toString()}`,
    );
  }

  // ─── Hybrid Retrieval ──────────────────────────────────────

  hybridSearch(
    query: string,
    opts: Partial<RetrievalRequest> = {},
  ): Promise<HybridResponse> {
    const body: RetrievalRequest = {
      query_text: query,
      session_id: this._sessionId,
      typed_predicates: [],
      clearance: ["public"],
      top_k: 20,
      time_window: null,
      scoring_profile: "default",
      require_certificate: false,
      include_test_origin: true,
      include_quarantined: false,
      require_provenance_verified: false,
      now: null,
      scoped_claim_ids: null,
      ...opts,
    };
    return this._client.post<HybridResponse>(
      `/ws/${this._workspace}/search/hybrid`,
      body,
    );
  }

  // ─── RARP / Active Engram Protocol ─────────────────────────

  materializeEngram(
    topic: string,
    opts: { seedEntityIds?: string[]; scope?: EngramScope } = {},
  ): Promise<MaterializeResponse> {
    const body: Record<string, unknown> = { topic };
    if (opts.seedEntityIds) body["seed_entity_ids"] = opts.seedEntityIds;
    if (opts.scope) body["scope"] = opts.scope;
    return this._client.post<MaterializeResponse>(
      `/ws/${this._workspace}/engrams`,
      body,
      { [SESSION_HEADER]: this._sessionId },
    );
  }

  probe(
    pointer: string,
    question: string,
    opts: {
      clearance?: string[];
      probeKind?: string;
      scoreWithHybrid?: boolean;
    } = {},
  ): Promise<ProbeAnswer> {
    const body: Record<string, unknown> = {
      question,
      score_with_hybrid: opts.scoreWithHybrid ?? false,
    };
    if (opts.clearance) body["clearance"] = opts.clearance;
    if (opts.probeKind) body["probe_kind"] = opts.probeKind;
    return this._client.post<ProbeAnswer>(
      `/ws/${this._workspace}/engrams/${encodeURIComponent(pointer)}/probe`,
      body,
      { [SESSION_HEADER]: this._sessionId },
    );
  }

  engrams(): Promise<EngramRef[]> {
    return this._client.get<EngramRef[]>(`/ws/${this._workspace}/engrams`, {
      [SESSION_HEADER]: this._sessionId,
    });
  }

  async expire(pointer: string): Promise<boolean> {
    const result = await this._client.del<{ expired: boolean; pointer: string }>(
      `/ws/${this._workspace}/engrams/${encodeURIComponent(pointer)}`,
      { [SESSION_HEADER]: this._sessionId },
    );
    return result.expired;
  }

  /**
   * Drop every engram in this Brain's session.  Idempotent.
   */
  async resetSession(): Promise<void> {
    const refs = await this.engrams();
    for (const ref of refs) {
      try {
        await this.expire(ref.pointer);
      } catch (err) {
        if (err instanceof ApiError && err.statusCode === 404) {
          // Already gone — fine.
          continue;
        }
        throw err;
      }
    }
  }
}

// ─── Module-private helpers ──────────────────────────────────

async function resolveFirstWorkspace(client: Client): Promise<string> {
  const workspaces = await client.get<WorkspaceInfo[]>("/workspaces");
  if (workspaces.length === 0) {
    throw new ApiError(
      404,
      "NO_WORKSPACE",
      "No workspaces mounted on the daemon",
    );
  }
  return workspaces[0]!.name;
}

function newSessionId(): string {
  return `ts-${randomBytes(8).toString("hex")}`;
}

/**
 * Spawn `root <args>`, capture stdout, parse JSON.  Used by
 * {@link Brain.mount}.
 */
function runRootCli<T>(args: readonly string[]): Promise<T> {
  return new Promise((resolve, reject) => {
    const child = spawn("root", args, { stdio: ["ignore", "pipe", "pipe"] });
    const stdoutChunks: Buffer[] = [];
    const stderrChunks: Buffer[] = [];

    child.stdout.on("data", (chunk: Buffer) => stdoutChunks.push(chunk));
    child.stderr.on("data", (chunk: Buffer) => stderrChunks.push(chunk));

    child.on("error", (err) => {
      if ((err as NodeJS.ErrnoException).code === "ENOENT") {
        reject(
          new ConnectionError(
            "`root` binary not on $PATH. Install via `cargo install thinkingroot-cli`.",
            err,
          ),
        );
      } else {
        reject(new ConnectionError(`spawn root: ${err.message}`, err));
      }
    });

    child.on("close", (code) => {
      if (code !== 0) {
        const stderr = Buffer.concat(stderrChunks).toString("utf-8");
        reject(
          new ConnectionError(
            `root ${args.join(" ")} exited ${code}: ${stderr}`,
          ),
        );
        return;
      }
      const stdout = Buffer.concat(stdoutChunks).toString("utf-8");
      try {
        resolve(JSON.parse(stdout) as T);
      } catch (err) {
        reject(
          new ConnectionError(
            `root ${args.join(" ")} produced non-JSON output: ${stdout}`,
            err,
          ),
        );
      }
    });
  });
}
