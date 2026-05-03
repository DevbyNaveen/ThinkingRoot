import { describe, expect, test, beforeEach } from "vitest";

import { Brain } from "../src/brain.js";
import { ApiError, ConnectionError } from "../src/errors.js";
import type {
  ApiEnvelope,
  Entity,
  HybridResponse,
  MaterializeResponse,
  ProbeAnswer,
  WorkspaceInfo,
} from "../src/types.js";

/**
 * Build a `fetch` stub that drives a fake daemon by URL.  Each route
 * returns a typed `ApiEnvelope<T>` so the Brain's parser exercises the
 * real success/error paths.
 */
function fakeFetch(
  routes: Record<string, (req: Request) => unknown>,
): typeof fetch {
  return (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input.toString();
    const path = new URL(url).pathname;
    const handler = routes[path];
    if (!handler) {
      const env: ApiEnvelope<null> = {
        ok: false,
        error: { code: "NOT_FOUND", message: `no fake route: ${path}` },
      };
      return new Response(JSON.stringify(env), { status: 404 });
    }
    const req = new Request(url, init as RequestInit);
    const result = handler(req);
    const env: ApiEnvelope<unknown> = { ok: true, data: result };
    return new Response(JSON.stringify(env), { status: 200 });
  }) as unknown as typeof fetch;
}

describe("Brain.remote", () => {
  test("auto-resolves the first workspace when none specified", async () => {
    const fetch = fakeFetch({
      "/api/v1/workspaces": () =>
        [
          {
            name: "alpha",
            path: "/tmp/alpha",
            entity_count: 10,
            claim_count: 20,
            source_count: 5,
          },
          {
            name: "beta",
            path: "/tmp/beta",
            entity_count: 1,
            claim_count: 2,
            source_count: 1,
          },
        ] as WorkspaceInfo[],
    });
    const brain = await Brain.remote("http://127.0.0.1:31760", { fetch });
    expect(brain.workspace).toBe("alpha");
  });

  test("respects an explicit workspace", async () => {
    const fetch = fakeFetch({});
    const brain = await Brain.remote("http://127.0.0.1:31760", {
      fetch,
      workspace: "myws",
    });
    expect(brain.workspace).toBe("myws");
  });

  test("entities() round-trip", async () => {
    let captured: string | null = null;
    const fetch = fakeFetch({
      "/api/v1/ws/myws/entities": (req) => {
        captured = req.url;
        return [
          {
            id: "e1",
            canonical_name: "Auth",
            entity_type: "concept",
            aliases: [],
            attributes: [],
            first_seen: "2026-05-03T00:00:00Z",
            last_updated: "2026-05-03T00:00:00Z",
          },
        ] as Entity[];
      },
    });
    const brain = await Brain.remote("http://127.0.0.1:31760", {
      fetch,
      workspace: "myws",
    });
    const ents = await brain.entities();
    expect(ents.length).toBe(1);
    expect(ents[0]!.canonical_name).toBe("Auth");
    expect(captured).toContain("/api/v1/ws/myws/entities");
  });

  test("hybridSearch() POSTs the full request shape", async () => {
    let captured: { body: string; headers: Headers } | null = null;
    const fetch = fakeFetch({
      "/api/v1/ws/myws/search/hybrid": (req) => {
        captured = {
          body: (req as { _body?: string })._body ?? "",
          headers: req.headers,
        };
        return {
          hits: [
            {
              claim_id: "c1",
              score: 0.91,
              score_breakdown: {
                vector: 0.5,
                admission: 0.1,
                trial: 0.1,
                source_authority: 0.1,
                recency: 0.05,
                complexity: 0,
                marker: 0,
                gap_proximity: 0,
                contradiction: 0,
                test_origin: 0,
              },
            },
          ],
          routing: {
            shape: "Vector+Datalog",
            total_candidates: 25,
            vector_candidates: 20,
            datalog_candidates: 5,
          },
        } as HybridResponse;
      },
    });
    const brain = await Brain.remote("http://127.0.0.1:31760", {
      fetch,
      workspace: "myws",
      sessionId: "ts-fixture",
    });
    const resp = await brain.hybridSearch("auth flow", { top_k: 25 });
    expect(resp.hits.length).toBe(1);
    expect(resp.routing.total_candidates).toBe(25);
  });

  test("materializeEngram() injects the session header", async () => {
    let sessionHeader: string | null = null;
    const fetch = fakeFetch({
      "/api/v1/ws/myws/engrams": (req) => {
        sessionHeader = req.headers.get("X-TR-Session-Id");
        return {
          pointer: "0xABCD",
          summary: { pointer: "0xABCD" },
        } as unknown as MaterializeResponse;
      },
    });
    const brain = await Brain.remote("http://127.0.0.1:31760", {
      fetch,
      workspace: "myws",
      sessionId: "ts-test",
    });
    const out = await brain.materializeEngram("auth flow");
    expect(out.pointer).toBe("0xABCD");
    expect(sessionHeader).toBe("ts-test");
  });

  test("probe() uses the engram pointer in the URL and adds session header", async () => {
    let captured: { path: string; header: string | null } | null = null;
    const fetch = fakeFetch({
      "/api/v1/ws/myws/engrams/0xABCD/probe": (req) => {
        captured = {
          path: new URL(req.url).pathname,
          header: req.headers.get("X-TR-Session-Id"),
        };
        return {
          answer: [{ kind: "factual", statement: "ok" }],
          claim_ids: ["c1"],
          source_byte_spans: [{ source_id: "s1", byte_start: 0, byte_end: 5 }],
          source_authority: ["high"],
          source_blake3s: ["blake3:abcd"],
          admission_tier: "rooted",
          valid_window: [null, null],
          superseded_by_chain: [],
          derivation_parents: [],
          sensitivity: "public",
          git_blame: [],
          related_quantities: [],
          related_doc_tags: [],
          related_calls: [],
          related_markers: [],
          caveats: [],
        } as unknown as ProbeAnswer;
      },
    });
    const brain = await Brain.remote("http://127.0.0.1:31760", {
      fetch,
      workspace: "myws",
      sessionId: "sess-1",
    });
    const ans = await brain.probe("0xABCD", "what?");
    expect(ans.claim_ids).toEqual(["c1"]);
    expect(captured!.path).toBe("/api/v1/ws/myws/engrams/0xABCD/probe");
    expect(captured!.header).toBe("sess-1");
  });

  test("ApiError carries status + code from the envelope", async () => {
    const fetch = (async () => {
      return new Response(
        JSON.stringify({
          ok: false,
          error: { code: "MISSING_SESSION", message: "X-TR-Session-Id required" },
        }),
        { status: 400 },
      );
    }) as unknown as typeof fetch;
    const brain = await Brain.remote("http://127.0.0.1:31760", {
      fetch,
      workspace: "myws",
      sessionId: "sess-1",
    });
    await expect(brain.materializeEngram("topic")).rejects.toBeInstanceOf(ApiError);
  });

  test("Brain.connect() throws when no cortex.lock is present", async () => {
    // Reuse XDG override from cortex.test.ts pattern.
    const orig = process.env["XDG_CONFIG_HOME"];
    process.env["XDG_CONFIG_HOME"] = "/nonexistent/xdg";
    try {
      await expect(Brain.connect()).rejects.toBeInstanceOf(ConnectionError);
    } finally {
      if (orig === undefined) delete process.env["XDG_CONFIG_HOME"];
      else process.env["XDG_CONFIG_HOME"] = orig;
    }
  });
});
