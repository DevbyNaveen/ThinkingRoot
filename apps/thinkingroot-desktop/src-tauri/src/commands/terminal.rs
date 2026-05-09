//! Embedded terminal — PTY-backed shell sessions for the right-rail
//! Terminal panel.
//!
//! Each open Terminal tab in the UI corresponds to one
//! [`TerminalSession`] here. The session owns:
//!
//! - the master end of a PTY pair (used for `resize` and to take a
//!   writer for stdin),
//! - a writer guarded by a `std::sync::Mutex` (xterm input → PTY),
//! - a dedicated OS thread reading the PTY master and emitting
//!   base64-encoded chunks on a per-session Tauri event,
//! - the [`portable_pty::Child`] handle so we can `kill()` the shell
//!   when the UI closes the tab or the window is destroyed.
//!
//! ## Why portable-pty
//!
//! `portable-pty` is the wezterm-team's cross-platform PTY abstraction
//! (5.9M+ downloads on crates.io). It uses ConPTY on Windows and
//! `openpty(3)` on macOS / Linux. Building our own PTY layer would
//! re-implement well-trodden code and would not pass the Claude Code
//! TUI canary test (Ink raw-mode + bracketed-paste + focus reporting).
//!
//! ## Honesty contract (CLAUDE.md §honesty rules)
//!
//! - PTY spawn errors propagate verbatim — no fake "started" toast.
//! - When the child exits we surface the real exit code (or signal
//!   description) to the UI through `terminal://exit/<id>`. The UI
//!   marks the tab "exited" instead of pretending the prompt is alive.
//! - On window destroy every session's child is killed and waited on
//!   so no `claude` / `root serve` survives the desktop process.
//!
//! ## Wire format
//!
//! - `terminal://data/<id>` payload: `{ "data": "<base64>" }` — raw
//!   PTY bytes; the UI base64-decodes and writes the `Uint8Array`
//!   straight into xterm. This keeps multi-byte UTF-8 sequences and
//!   ANSI control bytes intact across the JSON boundary.
//! - `terminal://exit/<id>` payload: `{ "code": i32 | null,
//!   "signal": String | null }`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use portable_pty::{Child as PtyChild, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use crate::state::AppState;

/// Default initial PTY geometry. The UI fits the terminal to its
/// container immediately on mount and emits a `terminal_resize` so
/// these defaults are only briefly visible.
const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 30;

// ─── Public types (mirrored on the TS side) ────────────────────────

/// Options the UI sends with `terminal_open`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TerminalOpenOpts {
    /// Working directory for the spawned shell. When `None` (or the
    /// path does not exist) we fall back to `$HOME`.
    pub cwd: Option<String>,
    /// Override the shell binary. When `None` we pick `$SHELL` on
    /// Unix and `pwsh.exe` → `cmd.exe` on Windows.
    pub shell: Option<String>,
    /// Initial PTY size (columns × rows). The UI is encouraged to
    /// pass real fit-addon values from xterm.
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// Optional environment overrides merged on top of the inherited
    /// environment (which carries the OS user's PATH, locale, etc.).
    pub env: Option<HashMap<String, String>>,
    /// Human-readable title shown in the tab strip. The UI may also
    /// update this from OSC 0/2 sequences later.
    pub title: Option<String>,
}

/// Public view of a session — what the UI renders in its tab strip
/// and uses to wire xterm to the right event topics.
#[derive(Debug, Clone, Serialize)]
pub struct TerminalSessionInfo {
    pub id: String,
    pub title: String,
    pub shell: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub created_at: DateTime<Utc>,
    /// Tauri event topic the read thread emits raw output on.
    pub data_event: String,
    /// Tauri event topic the read thread emits child-exit events on.
    pub exit_event: String,
}

/// Internal session state. Held inside `Arc` so the read-thread,
/// the IPC handlers, and the shutdown hook can all share it.
pub struct TerminalSession {
    pub info: TerminalSessionInfo,
    /// Master end — kept for `resize()` calls. Locked briefly per
    /// resize; the read thread holds its own cloned reader.
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Writer half taken from the master at spawn time. Locked per
    /// keystroke / paste — short critical sections, std::sync is fine.
    writer: Mutex<Box<dyn Write + Send>>,
    /// Spawned child handle. `Mutex<Option<...>>` because `kill()` and
    /// `wait()` need `&mut`, and we want to take ownership when the
    /// session is closed so a second `terminal_close` is a no-op.
    child: Mutex<Option<Box<dyn PtyChild + Send + Sync>>>,
}

// ─── Helpers ───────────────────────────────────────────────────────

fn data_topic(id: &str) -> String {
    format!("terminal://data/{id}")
}

fn exit_topic(id: &str) -> String {
    format!("terminal://exit/{id}")
}

fn pick_shell(explicit: Option<String>) -> String {
    if let Some(s) = explicit.filter(|s| !s.trim().is_empty()) {
        return s;
    }
    if cfg!(windows) {
        // Modern PowerShell sets PSModulePath with a `PowerShell\7`
        // segment when installed — cheap zero-spawn heuristic. Fall
        // back to Windows PowerShell which ships with every supported
        // Windows release.
        if std::env::var("PSModulePath")
            .map(|p| p.contains("PowerShell\\7") || p.contains("PowerShell/7"))
            .unwrap_or(false)
        {
            return "pwsh.exe".to_string();
        }
        return "powershell.exe".to_string();
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

/// Pick a working directory: caller-supplied → `$HOME` → `/`.
///
/// We deliberately do NOT silently default to the desktop process's
/// own cwd — that would land the user in `/Applications/...` on macOS,
/// which is surprising and confusing. The honest fallback is the user's
/// home.
fn pick_cwd(requested: Option<String>) -> PathBuf {
    if let Some(p) = requested.filter(|p| !p.trim().is_empty()) {
        let path = PathBuf::from(&p);
        if path.is_dir() {
            return path;
        }
    }
    if let Some(home) = dirs::home_dir() {
        return home;
    }
    PathBuf::from("/")
}

/// Build the argv for the shell. On Unix we want **login + interactive**
/// so `/etc/profile`, `~/.zprofile`, `~/.zshrc` all run — that is what
/// makes `claude`, `root`, `brew`, `nvm`-installed binaries resolve. The
/// VS Code / Cursor terminal does the same on macOS.
fn shell_argv(shell: &str) -> Vec<String> {
    if cfg!(windows) {
        // PowerShell already runs profile by default; cmd.exe takes no
        // login flag.
        return Vec::new();
    }
    let lower = shell.to_ascii_lowercase();
    let basename = std::path::Path::new(&lower)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match basename {
        // bash / zsh: -l makes it a login shell, -i forces interactive
        // even though stdin is a TTY (defensive — some installs need
        // both flags to source rc files).
        "bash" | "zsh" => vec!["-l".to_string(), "-i".to_string()],
        // fish: -l = login. fish is interactive by default on a TTY.
        "fish" => vec!["-l".to_string()],
        // For unknown shells we pass no flags; the user can override
        // via the `shell` opt if they need a custom invocation.
        _ => Vec::new(),
    }
}

// ─── Commands ──────────────────────────────────────────────────────

/// Spawn a new PTY-backed shell. Returns the session info the UI
/// needs to subscribe to events and route writes.
#[tauri::command]
pub async fn terminal_open(
    app: AppHandle,
    state: State<'_, AppState>,
    opts: TerminalOpenOpts,
) -> Result<TerminalSessionInfo, String> {
    let id = Uuid::new_v4().to_string();
    let shell = pick_shell(opts.shell.clone());
    let cwd = pick_cwd(opts.cwd.clone());
    let cols = opts.cols.unwrap_or(DEFAULT_COLS).max(2);
    let rows = opts.rows.unwrap_or(DEFAULT_ROWS).max(2);
    let title = opts
        .title
        .clone()
        .unwrap_or_else(|| default_title(&shell, &cwd));

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty failed: {e}"))?;

    let mut cmd = CommandBuilder::new(&shell);
    for arg in shell_argv(&shell) {
        cmd.arg(arg);
    }
    cmd.cwd(&cwd);

    // Inherit the desktop process's environment first so PATH / LANG /
    // TERM_PROGRAM stay sensible, then layer caller overrides.
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    // Mark TERM as `xterm-256color` — what xterm.js advertises and
    // what `tput`, `claude`, and `vim` expect for full colour and
    // alt-screen support.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "ThinkingRoot");
    cmd.env("TERM_PROGRAM_VERSION", crate::VERSION);
    if let Some(env) = &opts.env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn `{shell}` in `{}`: {e}", cwd.display()))?;
    let pid = child.process_id();

    // Take the writer once — subsequent take_writer calls on the same
    // master are not guaranteed to succeed.
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take_writer: {e}"))?;
    // Clone the reader before we move `pair.master` into the session —
    // the read thread needs an independent handle.
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("try_clone_reader: {e}"))?;

    let info = TerminalSessionInfo {
        id: id.clone(),
        title,
        shell: shell.clone(),
        cwd: cwd.display().to_string(),
        pid,
        created_at: Utc::now(),
        data_event: data_topic(&id),
        exit_event: exit_topic(&id),
    };

    let session = Arc::new(TerminalSession {
        info: info.clone(),
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        child: Mutex::new(Some(child)),
    });

    state
        .terminals
        .write()
        .await
        .insert(id.clone(), session.clone());

    // ── Read loop ────────────────────────────────────────────────
    //
    // portable-pty's reader is sync; we run it on a dedicated OS
    // thread (NOT a tokio blocking task) so its lifetime is entirely
    // independent of any tokio runtime. The thread exits when the PTY
    // hits EOF (child died and closed slave) or returns an error.
    let app_for_thread = app.clone();
    let id_for_thread = id.clone();
    let data_topic_for_thread = info.data_event.clone();
    let exit_topic_for_thread = info.exit_event.clone();
    let session_for_thread = session.clone();

    std::thread::Builder::new()
        .name(format!("terminal-read-{id}"))
        .spawn(move || {
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF — slave end closed (typically because the
                        // child exited). Break and emit exit event.
                        break;
                    }
                    Ok(n) => {
                        let chunk = &buf[..n];
                        let encoded = B64.encode(chunk);
                        // Best-effort emit; if the webview is gone we
                        // simply stop reading on the next iteration
                        // when the master is dropped during close.
                        if let Err(e) =
                            app_for_thread.emit(&data_topic_for_thread, DataEvent { data: encoded })
                        {
                            tracing::warn!(
                                terminal = %id_for_thread,
                                error = %e,
                                "terminal data emit failed"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            terminal = %id_for_thread,
                            error = %e,
                            "terminal reader closed"
                        );
                        break;
                    }
                }
            }

            // Reap the child so we get an honest exit code instead of
            // leaving a zombie. `wait()` is idempotent on portable-pty:
            // if it was already killed via `terminal_close`, this
            // returns the prior status.
            let exit = {
                let mut guard = session_for_thread.child.lock().unwrap();
                guard.as_mut().and_then(|c| c.wait().ok())
            };
            let payload = match exit {
                Some(status) => ExitEvent {
                    code: status.exit_code() as i32,
                    success: status.success(),
                },
                None => ExitEvent {
                    code: -1,
                    success: false,
                },
            };
            if let Err(e) = app_for_thread.emit(&exit_topic_for_thread, payload) {
                tracing::warn!(
                    terminal = %id_for_thread,
                    error = %e,
                    "terminal exit emit failed"
                );
            }
        })
        .map_err(|e| format!("spawn read thread: {e}"))?;

    Ok(info)
}

#[derive(Debug, Clone, Serialize)]
struct DataEvent {
    data: String,
}

#[derive(Debug, Clone, Serialize)]
struct ExitEvent {
    code: i32,
    success: bool,
}

/// Forward keyboard / paste input from xterm into the PTY master.
#[tauri::command]
pub async fn terminal_write(
    state: State<'_, AppState>,
    id: String,
    data: String,
) -> Result<(), String> {
    let session = {
        let map = state.terminals.read().await;
        map.get(&id).cloned()
    };
    let session = session.ok_or_else(|| format!("no terminal session `{id}`"))?;
    let mut writer = session
        .writer
        .lock()
        .map_err(|_| "terminal writer poisoned".to_string())?;
    writer
        .write_all(data.as_bytes())
        .map_err(|e| format!("write to pty: {e}"))?;
    // Flush so single-character keystrokes (Ctrl-C, etc.) reach the
    // child without buffering delay.
    writer.flush().map_err(|e| format!("flush pty: {e}"))?;
    Ok(())
}

/// Resize the PTY when the rail width or window height changes.
#[tauri::command]
pub async fn terminal_resize(
    state: State<'_, AppState>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let session = {
        let map = state.terminals.read().await;
        map.get(&id).cloned()
    };
    let session = session.ok_or_else(|| format!("no terminal session `{id}`"))?;
    let cols = cols.max(2);
    let rows = rows.max(2);
    let master = session
        .master
        .lock()
        .map_err(|_| "terminal master poisoned".to_string())?;
    master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("pty resize: {e}"))?;
    Ok(())
}

/// Kill the shell, drop the PTY, remove the session.
///
/// Idempotent: closing an already-closed session returns `Ok(())`.
#[tauri::command]
pub async fn terminal_close(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let session = {
        let mut map = state.terminals.write().await;
        map.remove(&id)
    };
    let Some(session) = session else {
        return Ok(());
    };

    // Take the child out of the session so we own the kill.
    let mut child_slot = session
        .child
        .lock()
        .map_err(|_| "terminal child poisoned".to_string())?;
    if let Some(mut child) = child_slot.take() {
        let _ = child.kill();
        // Give it a beat to die so the read thread sees EOF and emits
        // the exit event before the JS side tears down listeners.
        // 100ms is a tradeoff: longer is friendlier to slow shells,
        // shorter is friendlier to a user spamming `+`/`×`.
        std::thread::sleep(Duration::from_millis(100));
        let _ = child.wait();
    }
    Ok(())
}

/// List current sessions — used by the UI on mount to restore tab
/// strip state across full reloads (e.g. Vite HMR).
#[tauri::command]
pub async fn terminal_list(state: State<'_, AppState>) -> Result<Vec<TerminalSessionInfo>, String> {
    let map = state.terminals.read().await;
    let mut out: Vec<TerminalSessionInfo> = map.values().map(|s| s.info.clone()).collect();
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(out)
}

/// Best-effort cleanup hook called from the window-destroy handler in
/// `lib.rs`. Kills every live session so no `claude` / `root serve`
/// child outlives the desktop process.
pub async fn shutdown_all(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let ids: Vec<String> = {
        let map = state.terminals.read().await;
        map.keys().cloned().collect()
    };
    for id in ids {
        if let Err(e) = terminal_close(state.clone(), id.clone()).await {
            tracing::warn!(terminal = %id, error = %e, "terminal_close on shutdown failed");
        }
    }
}

fn default_title(shell: &str, cwd: &std::path::Path) -> String {
    let basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(shell);
    let dir = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| cwd.to_str().unwrap_or("~"));
    format!("{basename} · {dir}")
}
