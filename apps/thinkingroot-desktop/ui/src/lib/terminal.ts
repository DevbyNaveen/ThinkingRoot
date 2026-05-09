/**
 * Embedded terminal controller — pairs an xterm.js Terminal instance
 * with the Tauri PTY commands in `lib/tauri.ts`.
 *
 * Lifecycle (per session):
 *
 *   const c = await TerminalController.spawn({ cwd })
 *   c.attach(divElement)
 *   c.fitToContainer()                  ← also pushes resize to PTY
 *   …user types, runs `claude`, etc.…
 *   c.detach()                          ← keeps PTY alive across rail-tab switches
 *   await c.dispose()                   ← kills shell + frees xterm
 *
 * Why we own a *separate* class instead of doing this inline in the
 * React component: React's strict mode double-invokes effects, and
 * xterm's renderer construction is not idempotent. Holding the
 * controller in a `useRef` outside the effect keeps a single live
 * instance per session id.
 */
import { Terminal, type ITerminalOptions } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { ClipboardAddon } from "@xterm/addon-clipboard";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";

import {
  listenTerminalData,
  listenTerminalExit,
  terminalClose,
  terminalOpen,
  terminalResize,
  terminalWrite,
  type TerminalExitEvent,
  type TerminalOpenArgs,
  type TerminalSessionInfo,
} from "@/lib/tauri";

/**
 * Decode a base64 string into a Uint8Array. Defined inline to avoid
 * pulling another dep. xterm's `write()` accepts a `Uint8Array`
 * directly which preserves multibyte UTF-8 sequences and ANSI control
 * bytes split mid-chunk by the PTY reader — a `string`-only path
 * would break Claude Code / Ink's escape sequences.
 */
function decodeBase64(b64: string): Uint8Array {
  const bin = atob(b64);
  const len = bin.length;
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Theme tokens read from the host CSS so the terminal blends with
 *  the rest of the desktop shell. We sample once at construction and
 *  let `applyTheme()` rebuild on theme changes. */
function readThemeFromCss(): ITerminalOptions["theme"] {
  if (typeof window === "undefined") return undefined;
  const root = document.documentElement;
  const cs = window.getComputedStyle(root);
  // The `globals.css` token set uses HSL components in CSS variables,
  // wrapped in `hsl(var(--token))`. We resolve through a temporary
  // probe element so we get a real `rgb(...)` string xterm can use.
  const probe = (token: string) => {
    const el = document.createElement("span");
    el.style.color = `hsl(${cs.getPropertyValue(token).trim()})`;
    el.style.display = "none";
    root.appendChild(el);
    const value = window.getComputedStyle(el).color;
    root.removeChild(el);
    return value || "";
  };
  const background = probe("--background") || "#0b0d10";
  const foreground = probe("--foreground") || "#e6e8eb";
  const muted = probe("--muted-foreground") || "#9aa3ad";
  const accent = probe("--accent") || "#3b82f6";
  return {
    background,
    foreground,
    cursor: foreground,
    cursorAccent: background,
    selectionBackground: accent,
    selectionForeground: background,
    selectionInactiveBackground: muted,
  };
}

export interface TerminalControllerEvents {
  /** Fires once the child shell has exited. */
  onExit?: (info: TerminalExitEvent) => void;
  /** Fires when xterm reports a new title via OSC 0/2. */
  onTitle?: (title: string) => void;
  /** Fires after `attach()` sets up the renderer. */
  onAttached?: () => void;
}

export class TerminalController {
  readonly session: TerminalSessionInfo;
  private term: Terminal;
  private fit: FitAddon;
  private webgl: WebglAddon | null = null;
  private container: HTMLElement | null = null;
  private resizeObserver: ResizeObserver | null = null;
  private unlistenData: (() => void) | null = null;
  private unlistenExit: (() => void) | null = null;
  private dataDisposable: { dispose: () => void } | null = null;
  private titleDisposable: { dispose: () => void } | null = null;
  private events: TerminalControllerEvents = {};
  /** Last (cols, rows) we sent to the PTY — debounce identical resizes. */
  private lastSize: { cols: number; rows: number } | null = null;
  /** Set true once `dispose()` is called so late callbacks bail out. */
  private disposed = false;
  /** Set true after the shell exit event arrives. */
  exited = false;

  private constructor(session: TerminalSessionInfo) {
    this.session = session;
    this.term = new Terminal({
      // 13px fits naturally beside 12px sidebar copy and survives
      // rail width 250 → 800. Users can override via xterm's API
      // later if we expose a Terminal Settings sub-panel.
      fontFamily:
        '"JetBrains Mono", "MonoLisa", "SF Mono", Menlo, Consolas, ui-monospace, monospace',
      fontSize: 13,
      lineHeight: 1.2,
      cursorBlink: true,
      cursorStyle: "block",
      // Bracketed-paste + alt-screen + focus reporting are enabled by
      // default in xterm.js v6 — those are exactly the bits Claude
      // Code's Ink TUI relies on.
      allowProposedApi: true,
      scrollback: 5000,
      convertEol: false,
      macOptionIsMeta: true,
      rightClickSelectsWord: true,
      theme: readThemeFromCss(),
    });
    this.fit = new FitAddon();
    this.term.loadAddon(this.fit);
    this.term.loadAddon(new WebLinksAddon());
    this.term.loadAddon(new ClipboardAddon());
  }

  /** Spawn the underlying PTY and return a controller bound to it. */
  static async spawn(opts: TerminalOpenArgs = {}): Promise<TerminalController> {
    const session = await terminalOpen(opts);
    const c = new TerminalController(session);
    await c.subscribe();
    return c;
  }

  /** Bind to an existing session (reattach after rail-tab remount). */
  static async fromExisting(session: TerminalSessionInfo): Promise<TerminalController> {
    const c = new TerminalController(session);
    await c.subscribe();
    return c;
  }

  setEventHandlers(events: TerminalControllerEvents): void {
    this.events = events;
  }

  private async subscribe(): Promise<void> {
    this.unlistenData = await listenTerminalData(this.session.data_event, (chunk) => {
      if (this.disposed) return;
      this.term.write(decodeBase64(chunk.data));
    });
    this.unlistenExit = await listenTerminalExit(this.session.exit_event, (info) => {
      if (this.disposed) return;
      this.exited = true;
      this.term.write(
        `\r\n\x1b[2m[process exited with code ${info.code}]\x1b[0m\r\n`,
      );
      this.events.onExit?.(info);
    });

    // xterm input → PTY. xterm gives us properly-formed UTF-8 strings
    // for both keystrokes and pasted text; passing a string through
    // Tauri IPC is safe.
    this.dataDisposable = this.term.onData((data) => {
      if (this.exited) return;
      void terminalWrite(this.session.id, data).catch((err) => {
        console.error("[terminal] write failed", err);
      });
    });

    // OSC 0/2 title sequences — `claude`, vim, etc. set these.
    this.titleDisposable = this.term.onTitleChange((title) => {
      this.events.onTitle?.(title);
    });
  }

  /** Mount the xterm DOM into a container and start the WebGL renderer. */
  attach(container: HTMLElement): void {
    if (this.disposed) return;
    if (this.container === container) return;
    this.container = container;
    this.term.open(container);

    // Try WebGL renderer for ~3-5x throughput on large outputs (cat
    // big-file.json, Claude Code Ink rerenders). Fallback to canvas
    // automatically if the context is lost.
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => {
        webgl.dispose();
        this.webgl = null;
      });
      this.term.loadAddon(webgl);
      this.webgl = webgl;
    } catch (err) {
      console.warn("[terminal] WebGL renderer unavailable, using canvas", err);
    }

    // Track container size — fit on every resize so the rail drag
    // handle and window resize both reflow the terminal cleanly.
    this.resizeObserver = new ResizeObserver(() => {
      this.fitToContainer();
    });
    this.resizeObserver.observe(container);

    // Initial fit on the next animation frame so layout has settled.
    requestAnimationFrame(() => this.fitToContainer());
    this.events.onAttached?.();
  }

  /** Detach the DOM but keep the PTY alive. Used when the rail tab
   *  switches away — re-attach instead of re-spawning. */
  detach(): void {
    if (this.resizeObserver) {
      this.resizeObserver.disconnect();
      this.resizeObserver = null;
    }
    this.container = null;
  }

  fitToContainer(): void {
    if (this.disposed || !this.container) return;
    try {
      this.fit.fit();
    } catch {
      return;
    }
    const { cols, rows } = this.term;
    if (
      this.lastSize &&
      this.lastSize.cols === cols &&
      this.lastSize.rows === rows
    ) {
      return;
    }
    this.lastSize = { cols, rows };
    void terminalResize(this.session.id, cols, rows).catch((err) => {
      console.error("[terminal] resize failed", err);
    });
  }

  focus(): void {
    if (this.disposed) return;
    this.term.focus();
  }

  /** Re-read CSS variables and push a new theme to xterm. Call on
   *  app theme switch. */
  applyTheme(): void {
    if (this.disposed) return;
    this.term.options.theme = readThemeFromCss();
  }

  /** Programmatic input — used by "Run command" buttons that want to
   *  drop a `root install …` line into the active terminal. */
  runCommand(line: string): void {
    if (this.disposed || this.exited) return;
    void terminalWrite(this.session.id, `${line}\r`).catch((err) => {
      console.error("[terminal] runCommand failed", err);
    });
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    this.detach();
    this.dataDisposable?.dispose();
    this.titleDisposable?.dispose();
    this.unlistenData?.();
    this.unlistenExit?.();
    if (this.webgl) {
      try {
        this.webgl.dispose();
      } catch {
        /* ignore — context may already be lost */
      }
      this.webgl = null;
    }
    try {
      this.term.dispose();
    } catch {
      /* ignore */
    }
    if (!this.exited) {
      try {
        await terminalClose(this.session.id);
      } catch (err) {
        console.error("[terminal] close failed", err);
      }
    }
  }
}
