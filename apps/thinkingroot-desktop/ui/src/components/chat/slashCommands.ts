/**
 * Slash command dispatcher.
 *
 * Slash commands are intentionally narrow — each one maps to a single
 * Tauri call against the workspace. Long output ("here's the merge
 * diff") is folded into the toast or the MainPane's right rail rather
 * than dumped into chat history.
 */
import { toast } from "@/store/toast";
import {
  branchCheckout,
  branchCreate,
  branchList,
  branchMerge,
  workspaceCompile,
} from "@/lib/tauri";

export type SlashContext = {
  workspace: string;
  raw: string;
};

export async function runSlashCommand(ctx: SlashContext): Promise<void> {
  const trimmed = ctx.raw.trim();
  const [head, ...rest] = trimmed.slice(1).split(/\s+/);
  const arg = rest.join(" ").trim();

  switch ((head ?? "").toLowerCase()) {
    case "branch":
      return runBranch(ctx, arg);
    case "branches":
      return runBranches(ctx);
    case "checkout":
      return runCheckout(ctx, arg);
    case "merge":
      return runMerge(ctx, arg);
    case "compile":
    case "recompile":
      return runCompile(ctx);
    case "help":
      return runHelp();
    default:
      toast(`Unknown command: /${head}`, {
        kind: "warn",
        body: "Try /help to see available commands.",
      });
  }
}

async function runBranch(ctx: SlashContext, name: string) {
  if (!name) {
    toast("/branch requires a name", { kind: "warn" });
    return;
  }
  try {
    const b = await branchCreate({ workspace: ctx.workspace, name });
    toast(`Branch created: ${b.name}`, { kind: "success", body: `parent: ${b.parent}` });
  } catch (e) {
    toast("Create branch failed", {
      kind: "error",
      body: e instanceof Error ? e.message : String(e),
    });
  }
}

async function runBranches(ctx: SlashContext) {
  try {
    const list = await branchList(ctx.workspace);
    if (list.length === 0) {
      toast("No branches yet.", { kind: "info" });
      return;
    }
    const body = list
      .map((b) => `${b.current ? "→" : " "} ${b.name} (${b.status})`)
      .join("\n");
    toast(`${list.length} branch${list.length === 1 ? "" : "es"}`, {
      kind: "info",
      body,
    });
  } catch (e) {
    toast("List branches failed", {
      kind: "error",
      body: e instanceof Error ? e.message : String(e),
    });
  }
}

async function runCheckout(ctx: SlashContext, name: string) {
  if (!name) {
    toast("/checkout requires a branch name", { kind: "warn" });
    return;
  }
  try {
    await branchCheckout(ctx.workspace, name);
    toast(`HEAD → ${name}`, { kind: "success" });
  } catch (e) {
    toast("Checkout failed", {
      kind: "error",
      body: e instanceof Error ? e.message : String(e),
    });
  }
}

async function runMerge(ctx: SlashContext, arg: string) {
  if (!arg) {
    toast("/merge requires a branch name", { kind: "warn" });
    return;
  }
  const force = arg.includes("--force");
  const name = arg.replace(/--force/g, "").trim();
  try {
    const r = await branchMerge({ workspace: ctx.workspace, name, force });
    if (r.merged) {
      toast(`Merged ${name}`, {
        kind: "success",
        body: `+${r.new_claims} claims · ${r.auto_resolved} auto-resolved · ${r.conflicts} conflicts`,
      });
    } else {
      toast(`Merge blocked`, {
        kind: "warn",
        body: r.blocking_reasons.join("; ") || "see the diff for details",
      });
    }
  } catch (e) {
    toast("Merge failed", {
      kind: "error",
      body: e instanceof Error ? e.message : String(e),
    });
  }
}

async function runCompile(ctx: SlashContext) {
  try {
    await workspaceCompile({ target: ctx.workspace });
    toast(`Compile started`, {
      kind: "info",
      body: "Watch progress in the Brain tab.",
    });
  } catch (e) {
    toast("Compile failed to start", {
      kind: "error",
      body: e instanceof Error ? e.message : String(e),
    });
  }
}

function runHelp() {
  toast("Slash commands", {
    kind: "info",
    body: [
      "/branch <name> — fork a knowledge branch",
      "/branches — list branches",
      "/checkout <name> — switch HEAD",
      "/merge <name> [--force] — merge into main",
      "/compile — recompile the workspace",
    ].join("\n"),
  });
}
