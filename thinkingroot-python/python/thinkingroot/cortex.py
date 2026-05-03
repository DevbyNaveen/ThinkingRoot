"""Cortex Protocol discovery for the Python SDK.

The cortex daemon writes a JSON lockfile at
``<config_dir>/thinkingroot/cortex.lock`` whenever ``root serve`` (or
the Tauri desktop app) brings up the singleton engine.  This module
mirrors the Rust ``thinkingroot_core::cortex`` reader so Python-side
clients can discover an already-running daemon and skip spawning a
duplicate process — which, before cortex shipped, was the silent
CozoDB write-conflict bug.

The lockfile schema is reader-bumped: this module accepts
``schema_version <= SUPPORTED_SCHEMA``.  A future-versioned lockfile
raises ``IncompatibleLockSchema`` rather than silently
mis-interpreting unfamiliar fields.

Spec: ``docs/2026-05-02-unified-singleton-runtime.md`` §3.4.
"""

from __future__ import annotations

import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path

SUPPORTED_SCHEMA = 1
LIVENESS_PATH = "/livez"
DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 31760


class CortexError(Exception):
    """Raised when the cortex lockfile is malformed or unreadable."""


class IncompatibleLockSchema(CortexError):
    """Lockfile carries a schema_version newer than this client supports."""


@dataclass(frozen=True)
class CortexLock:
    """Snapshot of ``cortex.lock`` at read time."""

    schema_version: int
    pid: int
    port: int
    host: str
    version: str
    started_by: str
    started_at: str
    binary_path: str

    @property
    def base_url(self) -> str:
        """HTTP URL for the daemon (no trailing slash)."""
        return f"http://{self.host}:{self.port}"


def lock_path() -> Path:
    """Resolve ``<config_dir>/thinkingroot/cortex.lock``.

    Honors ``XDG_CONFIG_HOME`` on Linux for parity with the Rust
    ``dirs::config_dir()`` resolution.  On macOS uses ``~/Library/
    Application Support``; on Windows uses ``%APPDATA%``.  These three
    rules are exactly what ``dirs`` crate does — duplicating them here
    avoids a runtime dependency.
    """
    if sys.platform == "darwin":
        base = Path.home() / "Library" / "Application Support"
    elif sys.platform == "win32":
        appdata = os.environ.get("APPDATA")
        base = Path(appdata) if appdata else Path.home() / "AppData" / "Roaming"
    else:
        xdg = os.environ.get("XDG_CONFIG_HOME")
        base = Path(xdg) if xdg else Path.home() / ".config"
    return base / "thinkingroot" / "cortex.lock"


def read_lock() -> CortexLock | None:
    """Return the parsed lockfile, or ``None`` when none exists.

    Raises :class:`CortexError` on malformed / unreadable files, and
    :class:`IncompatibleLockSchema` when ``schema_version`` exceeds
    :data:`SUPPORTED_SCHEMA`.
    """
    path = lock_path()
    if not path.exists():
        return None
    try:
        with path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
    except OSError as exc:
        raise CortexError(f"read {path}: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise CortexError(f"parse {path}: {exc}") from exc

    schema = int(data.get("schema_version", 0))
    if schema > SUPPORTED_SCHEMA:
        raise IncompatibleLockSchema(
            f"cortex.lock schema_version={schema} exceeds supported "
            f"{SUPPORTED_SCHEMA} — upgrade `thinkingroot` package"
        )

    return CortexLock(
        schema_version=schema,
        pid=int(data["pid"]),
        port=int(data["port"]),
        host=str(data.get("host", DEFAULT_HOST)),
        version=str(data.get("version", "")),
        started_by=str(data.get("started_by", "")),
        started_at=str(data.get("started_at", "")),
        binary_path=str(data.get("binary_path", "")),
    )


def process_alive(pid: int) -> bool:
    """Return True if the OS reports the PID as alive.

    POSIX uses ``kill(pid, 0)``: an ``OSError`` with ``EPERM`` means
    the process exists but we don't own it (still alive).  Windows
    uses ``OpenProcess`` via ctypes.  Mirrors the lightweight check
    on the Rust side (``sysinfo`` with the lightest refresh).
    """
    if pid <= 0:
        return False
    if sys.platform == "win32":
        # ctypes-only: ship without a numpy/win32api dependency.
        import ctypes

        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        STILL_ACTIVE = 259
        kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
        handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not handle:
            return False
        try:
            exit_code = ctypes.c_ulong()
            ok = kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code))
            return bool(ok) and exit_code.value == STILL_ACTIVE
        finally:
            kernel32.CloseHandle(handle)
    else:
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        except OSError:
            return False
