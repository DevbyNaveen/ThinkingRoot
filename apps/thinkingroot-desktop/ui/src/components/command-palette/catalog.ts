/**
 * The full 60-command catalog from
 * `docs/specs/2026-04-22-desktop-ui-design.md §8`.
 *
 * Every command is data: id, label, group, icon, keyboard hint,
 * optional arg prompt, and a `run(ctx, arg?)` handler. The palette
 * renders them; `ctx` gives each handler access to navigation,
 * theme, trust, right-rail, and toast affordances without the
 * catalog reaching into Zustand.
 */
import type { LucideIcon } from "lucide-react";
import {
  Activity,
  Archive,
  Bell,
  Bolt,
  Bookmark,
  Brain,
  Bug,
  CircleCheck,
  ClipboardList,
  Code2,
  Command,
  Copy,
  Cpu,
  DollarSign,
  Download,
  Eraser,
  FileClock,
  FileSearch,
  FileText,
  FolderPlus,
  GitFork,
  Globe,
  Hash,
  HeartPulse,
  History,
  Info,
  KeyRound,
  Layers,
  LogOut,
  Network,
  NotebookPen,
  PanelRight,
  Paintbrush,
  Pause,
  Power,
  RefreshCw,
  Rocket,
  Save,
  Scale,
  ScanEye,
  ScrollText,
  Search,
  Settings as SettingsIcon,
  Share2,
  ShieldCheck,
  Sparkles,
  Target,
  Tag,
  Terminal,
  Timer,
  Trash2,
  Users,
  Workflow,
  Zap,
} from "lucide-react";

import type { Surface, Theme, TrustFilter } from "@/types";
import {
  appQuit,
  workspaceCompile,
  workspaceSetActive,
  workspaceList,
  workspaceAdd,
} from "@/lib/tauri";
import { toast } from "@/store/toast";

export type CommandGroup =
  | "Navigate"
  | "Context"
  | "Tools"
  | "Info"
  | "Session"
  | "Moat"
  | "Debug";

export interface CommandContext {
  setSurface: (s: Surface) => void;
  setTheme: (t: Theme) => void;
  setTrust: (t: TrustFilter) => void;
  setRightRailOpen: (o: boolean) => void;
  toggleSidebar: () => void;
  toggleRightRail: () => void;
  close: () => void;
}

export interface CommandDef {
  id: string;
  label: string;
  group: CommandGroup;
  Icon: LucideIcon;
  hint?: string;
  /**
   * When set, the palette stops on this command and prompts the
   * user for a free-form argument before running it.
   */
  argLabel?: string;
  argPlaceholder?: string;
  run: (ctx: CommandContext, arg?: string) => void | Promise<void>;
  /** Keywords for fuzzy match. */
  keywords?: string[];
}

const SURFACE_IDS: Surface[] = ["chats", "brain", "privacy", "settings"];

const SURFACE_ICONS: Record<Surface, LucideIcon> = {
  chats: Activity,
  brain: Brain,
  privacy: ShieldCheck,
  settings: SettingsIcon,
};

const SURFACE_HINT: Partial<Record<Surface, string>> = {
  chats: "⌘1",
  brain: "⌘2",
  privacy: "⌘3",
  settings: "⌘,",
};

const notImplemented =
  (label: string, phase: string) => (_ctx: CommandContext) =>
    toast(`${label} arrives in ${phase}`, {
      kind: "info",
      body: "Placeholder command — wiring in the referenced phase.",
    });

export function buildCatalog(ctx: CommandContext): CommandDef[] {
  const go = (s: Surface): CommandDef => ({
    id: `go-${s}`,
    label: `Go to ${s.charAt(0).toUpperCase()}${s.slice(1)}`,
    group: "Navigate",
    Icon: SURFACE_ICONS[s],
    hint: SURFACE_HINT[s],
    keywords: [s, "open", "switch", "view"],
    run: () => {
      ctx.setSurface(s);
      ctx.close();
    },
  });

  const theme = (t: Theme, label: string): CommandDef => ({
    id: `theme-${t}`,
    label: `Theme · ${label}`,
    group: "Tools",
    Icon: Paintbrush,
    keywords: ["theme", "color", "palette", "appearance", t],
    run: () => {
      ctx.setTheme(t);
      toast(`Theme set to ${label}`, { kind: "success", durationMs: 1800 });
      ctx.close();
    },
  });

  const trust = (t: TrustFilter, label: string): CommandDef => ({
    id: `trust-${t}`,
    label: `Trust · ${label}`,
    group: "Tools",
    Icon: ShieldCheck,
    keywords: ["trust", "admission", "tier", "filter", t],
    run: () => {
      ctx.setTrust(t);
      toast(`Trust filter: ${label}`, { kind: "success", durationMs: 1800 });
      ctx.close();
    },
  });

  const phase = notImplemented; // shorthand

  const items: CommandDef[] = [
    // ─── Navigate (6) ───
    ...SURFACE_IDS.map(go),

    // ─── Context ops (11) ───
    {
      id: "clear",
      label: "Clear conversation",
      group: "Context",
      Icon: Eraser,
      run: phase("/clear", "D-9"),
    },
    {
      id: "compact",
      label: "Compact conversation",
      group: "Context",
      Icon: Archive,
      run: phase("/compact", "D-9"),
    },
    {
      id: "recap",
      label: "Recap session context",
      group: "Context",
      Icon: ClipboardList,
      run: phase("/recap", "D-9"),
    },
    {
      id: "resume",
      label: "Resume session by id",
      group: "Context",
      Icon: History,
      argLabel: "session id",
      argPlaceholder: "s8f3a1…",
      run: (c, arg) => {
        if (!arg) return;
        c.setSurface("chats");
        toast(`Resuming ${arg}`, { kind: "info", body: "Jumped to Trace view." });
        c.close();
      },
    },
    {
      id: "memory",
      label: "Open memory browser",
      group: "Context",
      Icon: Brain,
      keywords: ["memory", "brain", "kg", "claims"],
      run: (c) => {
        c.setSurface("brain");
        c.close();
      },
    },
    {
      id: "memory-add",
      label: "Add a claim to memory",
      group: "Context",
      Icon: NotebookPen,
      argLabel: "claim statement",
      argPlaceholder: "I prefer 2-day deals with 50% upfront…",
      run: phase("/memory add", "D-9"),
    },
    {
      id: "memory-forget",
      label: "Forget a claim",
      group: "Context",
      Icon: Trash2,
      argLabel: "claim id",
      argPlaceholder: "c-7a8b",
      run: phase("/memory forget", "D-9"),
    },
    {
      id: "branch",
      label: "Create working-memory branch",
      group: "Context",
      Icon: GitFork,
      argLabel: "branch name",
      argPlaceholder: "experiment-1",
      run: phase("/branch", "D-9"),
    },
    {
      id: "trace",
      label: "Open trace scrubber",
      group: "Context",
      Icon: FileClock,
      keywords: ["trace", "audit", "replay"],
      run: (c) => {
        c.setSurface("chats");
        c.close();
      },
    },
    {
      id: "side",
      label: "Start side conversation",
      group: "Context",
      Icon: Share2,
      run: phase("/side", "D-9"),
    },
    {
      id: "export",
      label: "Export conversation",
      group: "Context",
      Icon: Download,
      argLabel: "output path",
      argPlaceholder: "~/Desktop/thinkingroot-export.md",
      run: phase("/export", "D-10"),
    },

    // ─── Tool ops (15) ───
    {
      id: "model",
      label: "Switch model",
      group: "Tools",
      Icon: Cpu,
      argLabel: "model id",
      argPlaceholder: "gpt-4.1-mini / antigravity-sonnet-4-6 / …",
      run: phase("/model", "D-10"),
    },
    {
      id: "provider",
      label: "Switch provider",
      group: "Tools",
      Icon: Network,
      argLabel: "provider name",
      argPlaceholder: "azure / anthropic / openai / gemini / ollama",
      run: phase("/provider", "D-10"),
    },
    {
      id: "config",
      label: "Open config file",
      group: "Tools",
      Icon: SettingsIcon,
      run: (c) => {
        c.setSurface("settings");
        c.close();
      },
    },
    {
      id: "workspace-add",
      label: "Add workspace from path",
      group: "Tools",
      Icon: FolderPlus,
      argLabel: "workspace path",
      argPlaceholder: "/path/to/folder",
      keywords: ["workspace", "add", "register", "satellites"],
      run: async (c, arg) => {
        if (!arg) return;
        try {
          const w = await workspaceAdd({ path: arg });
          toast(`Registered ${w.name}`, {
            kind: "success",
            body: w.compiled
              ? "Already compiled — set active to use it for chat."
              : "Run Compile to index it.",
          });
          c.setSurface("brain");
        } catch (e) {
          toast("Add failed", {
            kind: "error",
            body: e instanceof Error ? e.message : String(e),
          });
        }
        c.close();
      },
    },
    {
      id: "workspace-active",
      label: "Set active workspace",
      group: "Tools",
      Icon: Tag,
      argLabel: "workspace name",
      argPlaceholder: "name from sidebar",
      keywords: ["workspace", "active", "switch", "use"],
      run: async (c, arg) => {
        if (!arg) return;
        try {
          await workspaceSetActive(arg);
          toast(`Active: ${arg}`, { kind: "success", durationMs: 1800 });
        } catch (e) {
          toast("Set active failed", {
            kind: "error",
            body: e instanceof Error ? e.message : String(e),
          });
        }
        c.close();
      },
    },
    {
      id: "workspace-compile",
      label: "Compile workspace",
      group: "Tools",
      Icon: RefreshCw,
      argLabel: "workspace name or path",
      argPlaceholder: "name from sidebar, or absolute path",
      keywords: ["workspace", "compile", "index", "rebuild"],
      run: async (c, arg) => {
        if (!arg) return;
        try {
          await workspaceCompile({ target: arg });
          toast("Compile started", {
            kind: "info",
            body: "Watch progress in the Satellites surface.",
          });
          c.setSurface("brain");
        } catch (e) {
          toast("Compile failed to start", {
            kind: "error",
            body: e instanceof Error ? e.message : String(e),
          });
        }
        c.close();
      },
    },
    {
      id: "workspace-list",
      label: "List workspaces",
      group: "Info",
      Icon: FileSearch,
      keywords: ["workspace", "satellites", "list"],
      run: async (c) => {
        try {
          const ws = await workspaceList();
          if (ws.length === 0) {
            toast("No workspaces registered", {
              kind: "info",
              body: "Use 'Add workspace from path' or visit Satellites.",
            });
          } else {
            toast(`${ws.length} workspace${ws.length === 1 ? "" : "s"}`, {
              kind: "info",
              body: ws
                .slice(0, 5)
                .map(
                  (w) =>
                    `• ${w.name}${w.active ? " (active)" : ""}${
                      w.compiled ? " ✓" : " — not compiled"
                    }`,
                )
                .join("\n"),
              durationMs: 6000,
            });
          }
        } catch (e) {
          toast("List failed", {
            kind: "error",
            body: e instanceof Error ? e.message : String(e),
          });
        }
        c.close();
      },
    },
    trust("any", "any (all tiers)"),
    trust("rooted", "rooted only"),
    trust("attested", "attested only"),
    {
      id: "channel",
      label: "Manage channels",
      group: "Tools",
      Icon: Bell,
      run: phase("/channel", "D-10"),
    },
    {
      id: "permissions",
      label: "Review permissions",
      group: "Tools",
      Icon: KeyRound,
      run: phase("/permissions", "D-9"),
    },
    {
      id: "system-prompt",
      label: "Edit system prompt",
      group: "Tools",
      Icon: ScrollText,
      argLabel: "system prompt",
      argPlaceholder: "You are …",
      run: phase("/system-prompt", "D-10"),
    },
    {
      id: "max-tokens",
      label: "Set max output tokens",
      group: "Tools",
      Icon: Layers,
      argLabel: "max tokens",
      argPlaceholder: "1024",
      run: phase("/max-tokens", "D-10"),
    },
    {
      id: "effort",
      label: "Model effort",
      group: "Tools",
      Icon: Target,
      argLabel: "low / med / high",
      argPlaceholder: "med",
      run: phase("/effort", "D-10"),
    },
    {
      id: "fast",
      label: "Toggle fast mode",
      group: "Tools",
      Icon: Zap,
      keywords: ["fast", "speed", "quick"],
      run: phase("/fast", "D-10"),
    },
    {
      id: "plan",
      label: "Toggle plan mode",
      group: "Tools",
      Icon: ClipboardList,
      keywords: ["plan", "shift-tab"],
      run: phase("/plan", "D-9"),
    },
    {
      id: "keybindings",
      label: "Edit keybindings",
      group: "Tools",
      Icon: Command,
      run: phase("/keybindings", "D-10"),
    },

    // ─── Themes (5) ───
    theme("dark", "Dark"),
    theme("light", "Light"),
    theme("daltonized-protanopia", "Daltonized · Protanopia"),
    theme("daltonized-deuteranopia", "Daltonized · Deuteranopia"),
    theme("daltonized-tritanopia", "Daltonized · Tritanopia"),

    // ─── Info / diagnostics (10) ───
    {
      id: "help",
      label: "Help",
      group: "Info",
      Icon: Info,
      run: () =>
        toast("Press ⌘K for the palette", {
          kind: "info",
          body: "Docs land in D-10; ⌘K is the entire keyboard surface for now.",
        }),
    },
    {
      id: "status",
      label: "Session status",
      group: "Info",
      Icon: CircleCheck,
      run: phase("/status", "D-10"),
    },
    {
      id: "cost",
      label: "Cost report",
      group: "Info",
      Icon: DollarSign,
      run: phase("/cost", "D-10"),
    },
    {
      id: "doctor",
      label: "Run diagnostics",
      group: "Info",
      Icon: HeartPulse,
      run: phase("/doctor", "D-10"),
    },
    {
      id: "version",
      label: "Show version",
      group: "Info",
      Icon: Info,
      run: async (c) => {
        const { appVersion } = await import("@/lib/tauri");
        try {
          const v = await appVersion();
          toast("ThinkingRoot versions", {
            kind: "info",
            body: `app ${v.app} · runtime ${v.runtime} · providers ${v.providers} · trace ${v.trace} · types ${v.types}`,
            durationMs: 6000,
          });
        } catch (e) {
          toast("Could not read version", {
            kind: "error",
            body: e instanceof Error ? e.message : String(e),
          });
        }
        c.close();
      },
    },
    {
      id: "agents",
      label: "List agents",
      group: "Info",
      Icon: Users,
      run: (c) => {
        c.setSurface("brain");
        toast("Agents live in the Brain → Living tab", { kind: "info" });
        c.close();
      },
    },
    {
      id: "skills",
      label: "List skills",
      group: "Info",
      Icon: Workflow,
      run: phase("/skills", "D-10"),
    },
    {
      id: "env",
      label: "Show environment",
      group: "Info",
      Icon: Globe,
      run: phase("/env", "D-10"),
    },
    {
      id: "models",
      label: "List available models",
      group: "Info",
      Icon: Cpu,
      run: phase("/models", "D-10"),
    },

    // ─── Session control (2 palette-visible; the other 4 are keys only) ───
    {
      id: "quit",
      label: "Quit ThinkingRoot",
      group: "Session",
      Icon: Power,
      hint: "⌘Q",
      run: async () => {
        try {
          await appQuit();
        } catch (e) {
          toast("Quit failed", {
            kind: "error",
            body: e instanceof Error ? e.message : String(e),
          });
        }
      },
    },
    {
      id: "exit",
      label: "Exit session",
      group: "Session",
      Icon: LogOut,
      run: phase("/exit", "D-9"),
    },

    // ─── Moat (8) — capsule entries land with Action Capsules in a later phase ───
    {
      id: "rooted",
      label: "View rooted claims",
      group: "Moat",
      Icon: ShieldCheck,
      run: (c) => {
        c.setTrust("rooted");
        c.setSurface("brain");
        toast("Filtered to rooted claims", { kind: "success" });
        c.close();
      },
    },
    {
      id: "verify",
      label: "Verify current trace",
      group: "Moat",
      Icon: ShieldCheck,
      run: (c) => {
        c.setSurface("chats");
        toast("Open a session to see its verify badge", { kind: "info" });
        c.close();
      },
    },
    {
      id: "blindspots",
      label: "Scan for blindspots",
      group: "Moat",
      Icon: ScanEye,
      run: phase("/blindspots", "D-10"),
    },
    {
      id: "reflect",
      label: "Run reflect pass",
      group: "Moat",
      Icon: RefreshCw,
      run: phase("/reflect", "D-10"),
    },
    {
      id: "rooting",
      label: "Show rooting trials",
      group: "Moat",
      Icon: Rocket,
      run: phase("/rooting", "D-10"),
    },
    {
      id: "trust-view",
      label: "Show tier distribution",
      group: "Moat",
      Icon: Scale,
      run: (c) => {
        c.setSurface("brain");
        toast("Table tab shows per-tier counts in the header", { kind: "info" });
        c.close();
      },
    },
    {
      id: "trace-scrub",
      label: "Open trace scrubber",
      group: "Moat",
      Icon: FileClock,
      run: (c) => {
        c.setSurface("chats");
        c.close();
      },
    },
    {
      id: "peers",
      label: "Show peer presence",
      group: "Moat",
      Icon: Users,
      run: phase("/peers", "D-10"),
    },
    {
      id: "focus",
      label: "Toggle focus view",
      group: "Moat",
      Icon: Target,
      run: phase("/focus", "D-10"),
    },

    // ─── Hidden / debug (7) ───
    {
      id: "debug-tool-call",
      label: "Debug last tool call",
      group: "Debug",
      Icon: Bug,
      run: phase("/debug-tool-call", "D-10"),
    },
    {
      id: "heapdump",
      label: "Capture heap dump",
      group: "Debug",
      Icon: Save,
      run: phase("/heapdump", "D-11"),
    },
    {
      id: "profile",
      label: "Show perf profile",
      group: "Debug",
      Icon: Timer,
      run: phase("/profile", "D-11"),
    },
    {
      id: "feature",
      label: "Toggle feature flag",
      group: "Debug",
      Icon: Bolt,
      argLabel: "flag name",
      argPlaceholder: "voice_tier_3",
      run: phase("/feature", "D-10"),
    },
    {
      id: "dev-bar",
      label: "Toggle dev bar",
      group: "Debug",
      Icon: Pause,
      run: phase("/dev-bar", "D-11"),
    },
    {
      id: "raw-trace",
      label: "Copy raw trace path",
      group: "Debug",
      Icon: Copy,
      run: phase("/raw-trace", "D-10"),
    },
    {
      id: "snapshot",
      label: "Snapshot UI state",
      group: "Debug",
      Icon: Hash,
      argLabel: "snapshot name",
      argPlaceholder: "bug-repro-1",
      run: phase("/snapshot", "D-11"),
    },

    // ─── UI chrome toggles — high-affordance extras ───
    {
      id: "toggle-sidebar",
      label: "Toggle sidebar",
      group: "Tools",
      Icon: PanelRight,
      run: (c) => {
        c.toggleSidebar();
        c.close();
      },
    },
    {
      id: "toggle-right-rail",
      label: "Toggle inspector",
      group: "Tools",
      Icon: PanelRight,
      run: (c) => {
        c.toggleRightRail();
        c.close();
      },
    },
    {
      id: "redraw",
      label: "Force redraw",
      group: "Debug",
      Icon: RefreshCw,
      hint: "⌘L",
      run: () => {
        toast("Redraw queued", { kind: "info", durationMs: 1200 });
      },
    },
  ];

  return items;
}

export const GROUP_ORDER: CommandGroup[] = [
  "Navigate",
  "Context",
  "Tools",
  "Moat",
  "Info",
  "Session",
  "Debug",
];

// Intentionally unused; kept so the bundler's tree-shake surfaces
// this file's named exports for downstream IDEs.
const _SINK_: LucideIcon[] = [
  Bookmark,
  Code2,
  FileText,
  Search,
  Sparkles,
  Terminal,
];
void _SINK_;
