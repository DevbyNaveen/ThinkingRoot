import type { AgentStep } from "@/types";

export const TOOL_OUTPUT_PREVIEW_CHARS = 2_400;
export const TOOL_OUTPUT_EXPAND_CHARS = 12_000;

export function isThinkTool(name: string): boolean {
  return name === "think";
}

export function isShellTool(name: string): boolean {
  const n = name.toLowerCase();
  return (
    n.includes("shell") ||
    n === "bash" ||
    n === "run_command" ||
    n === "run-command"
  );
}

export function isFileTool(name: string): boolean {
  const n = name.toLowerCase();
  return (
    n.includes("file_read") ||
    n.includes("file_write") ||
    n.includes("file_edit") ||
    n === "read" ||
    n === "write"
  );
}

export function parseJsonLoose(raw: string): unknown | null {
  const t = raw.trim();
  if (!t.startsWith("{") && !t.startsWith("[")) return null;
  try {
    return JSON.parse(t) as unknown;
  } catch {
    return null;
  }
}

export function extractShellCommand(input: string): string | null {
  const parsed = parseJsonLoose(input);
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;
  const command = (parsed as Record<string, unknown>).command;
  return typeof command === "string" && command.trim() ? command.trim() : null;
}

export function extractFilePath(input: string): string | null {
  const parsed = parseJsonLoose(input);
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;
  const path = (parsed as Record<string, unknown>).path;
  return typeof path === "string" && path.trim() ? path.trim() : null;
}

export interface ShellOutputView {
  summary: string;
  body: string;
  lineCount: number;
  exitCode?: number;
}

export function formatShellOutput(output: string): ShellOutputView {
  const parsed = parseJsonLoose(output);
  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    const o = parsed as Record<string, unknown>;
    const stdout =
      typeof o.stdout === "string"
        ? o.stdout
        : typeof o.output === "string"
          ? o.output
          : typeof o.content === "string"
            ? o.content
            : "";
    const stderr = typeof o.stderr === "string" ? o.stderr : "";
    const exitCode =
      typeof o.exit_code === "number"
        ? o.exit_code
        : typeof o.code === "number"
          ? o.code
          : undefined;
    const body = [stdout, stderr && `--- stderr ---\n${stderr}`]
      .filter(Boolean)
      .join("\n");
    const lineCount = body ? body.split("\n").length : 0;
    const summary =
      exitCode !== undefined
        ? `${lineCount} line${lineCount === 1 ? "" : "s"} · exit ${exitCode}`
        : `${lineCount} line${lineCount === 1 ? "" : "s"}`;
    return { summary, body, lineCount, exitCode };
  }
  const body = output.trim();
  const lineCount = body ? body.split("\n").length : 0;
  return {
    summary: `${lineCount} line${lineCount === 1 ? "" : "s"}`,
    body,
    lineCount,
  };
}

export function truncateText(
  text: string,
  max = TOOL_OUTPUT_PREVIEW_CHARS,
): { text: string; truncated: boolean; totalChars: number } {
  if (text.length <= max) {
    return { text, truncated: false, totalChars: text.length };
  }
  return {
    text: `${text.slice(0, max)}\n…[${text.length - max} more characters]`,
    truncated: true,
    totalChars: text.length,
  };
}

/** One-line label for collapsed cards and the live evidence timeline. */
export function stepActivityLabel(step: AgentStep): string {
  if (isThinkTool(step.name)) return "Reasoning step";

  if (isShellTool(step.name)) {
    const cmd = extractShellCommand(step.input);
    if (cmd) {
      const flat = cmd.replace(/\s+/g, " ").trim();
      return flat.length > 88 ? `${flat.slice(0, 86)}…` : flat;
    }
    return "Shell command";
  }

  const path = extractFilePath(step.input);
  if (path) {
    return path.length > 88 ? `…${path.slice(-86)}` : path;
  }

  return friendlyToolTitle(step.name);
}

/** Short verb for inner-trace lines (Cursor / Claude Code style). */
export function shortToolVerb(name: string): string {
  const n = name.toLowerCase();
  if (n.includes("witness")) return "Witness";
  if (n.includes("relation") || n.includes("graph")) return "Graph";
  if (n.includes("search") || n.includes("query")) return "Search";
  if (n.includes("glob")) return "Glob";
  if (n.includes("grep")) return "Grep";
  if (isShellTool(name)) return "Shell";
  if (n.includes("file_read") || n === "read") return "Read";
  if (n.includes("file_write") || n.includes("file_edit") || n === "write") {
    return "Write";
  }
  if (n.includes("compile")) return "Compile";
  if (n.includes("claim")) return "Claim";
  return friendlyToolTitle(name);
}

export function labelForTool(name: string): string {
  const n = name.toLowerCase();
  if (n.includes("witness")) return "Checking witnesses";
  if (n.includes("relation") || n.includes("graph")) return "Reading graph context";
  if (n.includes("search") || n.includes("query")) return "Searching knowledge base";
  if (n.includes("shell") || n === "bash") return "Running shell command";
  if (n.includes("glob")) return "Finding files";
  if (n.includes("grep")) return "Searching files";
  if (n.includes("file_read") || n === "read") return "Reading file";
  if (n.includes("file_write") || n.includes("file_edit") || n === "write") {
    return "Writing file";
  }
  if (n.includes("compile")) return "Compiling workspace";
  if (n.includes("claim")) return "Reading relevant claims";
  if (n.includes("summar") || n.includes("synth")) return "Composing answer";
  return name.replace(/_/g, " ");
}

export function friendlyToolTitle(name: string): string {
  const n = name.toLowerCase();
  if (n.includes("witness")) return "Checking witnesses";
  if (n.includes("relation") || n.includes("graph")) return "Reading graph";
  if (n.includes("search") || n.includes("query")) return "Searching workspace";
  if (n.includes("glob")) return "Finding files";
  if (n.includes("grep")) return "Searching file contents";
  if (isShellTool(name)) return "Shell";
  if (n.includes("file_read") || n === "read") return "Reading file";
  if (n.includes("file_write") || n.includes("file_edit") || n === "write") {
    return "Writing file";
  }
  if (n.includes("compile")) return "Compiling workspace";
  if (n.includes("claim")) return "Reading claims";
  return name.replace(/_/g, " ");
}

/** Only approval gates start expanded — never finished shell dumps. */
export function shouldDefaultExpandStep(step: AgentStep): boolean {
  return step.status === "awaiting_approval";
}
