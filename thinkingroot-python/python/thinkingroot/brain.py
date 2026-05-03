"""High-level facade for ThinkingRoot in Python.

The :class:`Brain` class is the canonical user-facing entry point of
the SDK.  It abstracts over three transports:

============== ===========================================================
``open(path)``  In-process via PyO3.  Zero-network, sub-millisecond
                queries.  Best for notebook / data-science usage.
``remote(url)`` HTTP via httpx.  Best for distributed / containerized
                agents that talk to a central daemon.
``connect()``   Cortex-aware auto-discovery.  Reads the cortex lockfile
                — if a daemon is running, attaches; otherwise falls
                back to in-process.  Best for the "just works"
                developer experience.
============== ===========================================================

All three transports expose the same method surface (entities,
claims, search, hybrid_search, materialize_engram, probe, engrams,
expire) — so swapping transports is a one-line change.

Example::

    from thinkingroot import Brain

    brain = Brain.connect()
    pointer = brain.materialize_engram("auth flow")["pointer"]
    answer = brain.probe(pointer, "what changed last week?")
    for row, claim_id in zip(answer["answer"], answer["claim_ids"]):
        print(claim_id, row)

Spec: ``docs/secondary-brain-concept.md`` §4 (the SDK plug story).
"""

from __future__ import annotations

import secrets
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from thinkingroot import cortex
from thinkingroot.client import APIError, Client


@dataclass(frozen=True)
class BrainInfo:
    """Connection metadata returned by :meth:`Brain.info`."""

    transport: str
    workspace: str
    base_url: str | None
    session_id: str
    daemon_pid: int | None
    daemon_started_by: str | None


class Brain:
    """Unified facade over in-process and remote ThinkingRoot engines.

    Construct via the :meth:`open`, :meth:`remote`, :meth:`mount`, or
    :meth:`connect` class methods — never the ``__init__`` directly.
    """

    def __init__(
        self,
        *,
        transport: str,
        engine: Any | None,
        client: Client | None,
        workspace: str,
        session_id: str,
        base_url: str | None,
        daemon_pid: int | None = None,
        daemon_started_by: str | None = None,
    ):
        self._transport = transport
        self._engine = engine
        self._client = client
        self._workspace = workspace
        self._session_id = session_id
        self._base_url = base_url
        self._daemon_pid = daemon_pid
        self._daemon_started_by = daemon_started_by

    # ─── Constructors ──────────────────────────────────────────

    @classmethod
    def open(cls, path: str | Path) -> "Brain":
        """Open a compiled workspace in-process.

        The directory must already contain ``.thinkingroot/`` (i.e.
        a workspace produced by ``root compile`` or
        ``thinkingroot.compile()``).  For a ``.tr`` pack, use
        :meth:`mount` instead.
        """
        # Lazy import: native bindings may not be present in pure-HTTP
        # installs (e.g. wheels built with --no-default-features).
        from thinkingroot import open as _native_open

        engine = _native_open(str(path))
        # Workspace name = directory name, matching the Rust open()
        # behaviour.  The session id rides on the PyO3 Engine.
        ws_name = Path(path).resolve().name or "default"
        return cls(
            transport="in_process",
            engine=engine,
            client=None,
            workspace=ws_name,
            session_id=engine.session_id,
            base_url=None,
        )

    @classmethod
    def remote(
        cls,
        base_url: str = "http://127.0.0.1:31760",
        *,
        api_key: str | None = None,
        workspace: str | None = None,
        session_id: str | None = None,
    ) -> "Brain":
        """Attach to a running ``root serve`` daemon over HTTP.

        ``workspace`` defaults to the first workspace mounted on the
        daemon.  ``session_id`` defaults to a fresh per-Brain token —
        passing a stable value across processes lets multiple Brains
        share the same engram set.
        """
        client = Client(base_url=base_url, api_key=api_key)
        ws_name = workspace or _resolve_first_workspace(client)
        sid = session_id or _new_session_id()
        return cls(
            transport="remote",
            engine=None,
            client=client,
            workspace=ws_name,
            session_id=sid,
            base_url=base_url,
        )

    @classmethod
    def connect(
        cls,
        *,
        workspace: str | None = None,
        session_id: str | None = None,
    ) -> "Brain":
        """Cortex-aware auto-discovery.

        Reads the cortex lockfile.  If a daemon is alive at the
        recorded host:port, returns a :meth:`remote` Brain attached
        to it.  Otherwise raises :class:`ConnectionError` — no
        in-process fallback because that would defeat the
        single-writer guarantee cortex enforces (a Brain.connect()
        opening CozoDB while the daemon also has it open is the
        write-conflict bug cortex was built to prevent).
        """
        lock = cortex.read_lock()
        if lock is None:
            raise ConnectionError(
                "no cortex daemon running. Start one with `root serve` "
                "or use Brain.open(path) for in-process access."
            )
        if not cortex.process_alive(lock.pid):
            raise ConnectionError(
                f"cortex.lock points to pid {lock.pid} but the process is dead. "
                "Run `root serve` to start a fresh daemon."
            )
        return cls.remote(
            base_url=lock.base_url,
            workspace=workspace,
            session_id=session_id,
        ).with_daemon_meta(pid=lock.pid, started_by=lock.started_by)

    @classmethod
    def mount(
        cls,
        pack_path: str | Path,
        *,
        name: str | None = None,
        no_verify: bool = False,
        recompile: bool = False,
    ) -> "Brain":
        """Mount a ``.tr`` pack via the cortex daemon and attach.

        Convenience wrapper around the ``root mount`` CLI subcommand —
        spawns it as a subprocess, parses the printed ``MountSummary``
        JSON, then returns a :meth:`remote` Brain pointed at the
        freshly-mounted workspace.

        Requires the ``root`` binary on ``PATH``.  For pure in-process
        unpack-and-replay without a daemon, use the lower-level Rust
        ``mount_cmd`` — the daemon path is the supported "secondary
        brain plug" experience.
        """
        import json
        import shutil
        import subprocess

        binary = shutil.which("root")
        if binary is None:
            raise RuntimeError(
                "`root` binary not on PATH. Install via `cargo install "
                "thinkingroot-cli` or download a release artifact."
            )
        cmd = [binary, "mount", str(pack_path)]
        if name:
            cmd.extend(["--name", name])
        if no_verify:
            cmd.append("--no-verify")
        if recompile:
            cmd.append("--recompile")
        result = subprocess.run(
            cmd, capture_output=True, text=True, check=False
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"root mount failed (exit {result.returncode}): {result.stderr}"
            )
        try:
            summary = json.loads(result.stdout)
        except json.JSONDecodeError as exc:
            raise RuntimeError(
                f"root mount returned non-JSON output: {result.stdout!r}"
            ) from exc
        # rest_url shape: http://host:port/api/v1/ws/<name>/.  Strip
        # the /api/v1/ws/<name>/ tail so the Brain.remote(base_url)
        # gets the daemon root.
        rest = summary["rest_url"].rstrip("/")
        marker = "/api/v1/ws/"
        if marker in rest:
            base = rest.split(marker, 1)[0]
        else:
            base = rest
        return cls.remote(
            base_url=base,
            workspace=summary["workspace"],
        ).with_daemon_meta(
            pid=int(summary.get("daemon_pid", 0)),
            started_by="root_mount",
        )

    def with_daemon_meta(
        self,
        *,
        pid: int | None,
        started_by: str | None,
    ) -> "Brain":
        """Internal: stamp daemon provenance on a Brain."""
        self._daemon_pid = pid
        self._daemon_started_by = started_by
        return self

    # ─── Introspection ─────────────────────────────────────────

    @property
    def workspace(self) -> str:
        return self._workspace

    @property
    def session_id(self) -> str:
        return self._session_id

    @property
    def transport(self) -> str:
        return self._transport

    def info(self) -> BrainInfo:
        return BrainInfo(
            transport=self._transport,
            workspace=self._workspace,
            base_url=self._base_url,
            session_id=self._session_id,
            daemon_pid=self._daemon_pid,
            daemon_started_by=self._daemon_started_by,
        )

    # ─── Claims / Entities / Search ────────────────────────────

    def entities(self) -> list[dict]:
        if self._engine is not None:
            return self._engine.get_entities()
        return self._client.entities(workspace=self._workspace)

    def entity(self, name: str) -> dict:
        if self._engine is not None:
            return self._engine.get_entity(name)
        return self._client.entity(name, workspace=self._workspace)

    def claims(
        self,
        *,
        claim_type: str | None = None,
        min_confidence: float | None = None,
    ) -> list[dict]:
        if self._engine is not None:
            return self._engine.get_claims(
                **{"type": claim_type} if claim_type else {},
                min_confidence=min_confidence,
            )
        return self._client.claims(
            workspace=self._workspace,
            type=claim_type,
            min_confidence=min_confidence,
        )

    def relations(self, entity: str) -> list[dict]:
        if self._engine is not None:
            return self._engine.get_relations(entity)
        return self._client.relations(entity, workspace=self._workspace)

    def search(self, query: str, *, top_k: int = 10) -> dict:
        if self._engine is not None:
            return self._engine.search(query, top_k=top_k)
        return self._client.search(query, workspace=self._workspace, top_k=top_k)

    # ─── Hybrid Retrieval ──────────────────────────────────────

    def hybrid_search(
        self,
        query: str,
        *,
        top_k: int = 20,
        require_certificate: bool = False,
        include_quarantined: bool = False,
        require_provenance_verified: bool = False,
    ) -> dict:
        """Run the 11-component hybrid retrieval pipeline.

        Returns the full ``HybridResponse`` shape: a list of
        ``hits`` (each carrying ``claim_id``, ``score``,
        ``score_breakdown``, ``provenance``) plus diagnostic
        ``stage_timings`` and ``routing`` shape.
        """
        if self._engine is not None:
            return self._engine.hybrid_search(
                query,
                top_k=top_k,
                require_certificate=require_certificate,
                include_quarantined=include_quarantined,
                require_provenance_verified=require_provenance_verified,
            )
        # Remote: POST /api/v1/ws/{ws}/search/hybrid with the full
        # RetrievalRequest body shape (matches engine::RetrievalRequest
        # so the SDK and the engine speak the same wire schema).
        body = {
            "query_text": query,
            "typed_predicates": [],
            "session_id": self._session_id,
            "clearance": ["public"],
            "top_k": top_k,
            "time_window": None,
            "scoring_profile": "default",
            "require_certificate": require_certificate,
            "include_test_origin": True,
            "include_quarantined": include_quarantined,
            "require_provenance_verified": require_provenance_verified,
            "now": None,
            "scoped_claim_ids": None,
        }
        return self._client._post_json(
            f"/ws/{self._workspace}/search/hybrid", body
        )

    # ─── RARP / Active Engram Protocol ─────────────────────────

    def materialize_engram(
        self,
        topic: str,
        *,
        seed_entity_ids: list[str] | None = None,
        scope: dict | None = None,
    ) -> dict:
        """Build an Engram for ``topic``. Returns ``{pointer, summary}``."""
        if self._engine is not None:
            return self._engine.materialize_engram(
                topic, seed_entity_ids=seed_entity_ids, scope=scope
            )
        body = {"topic": topic}
        if seed_entity_ids is not None:
            body["seed_entity_ids"] = seed_entity_ids
        if scope is not None:
            body["scope"] = scope
        return self._client_post_with_session(
            f"/ws/{self._workspace}/engrams", body
        )

    def probe(
        self,
        pointer: str,
        question: str,
        *,
        clearance: list[str] | None = None,
        probe_kind: str | None = None,
        score_with_hybrid: bool = False,
    ) -> dict:
        """Probe an Engram with a natural-language question.

        Returns the ``ProbeAnswer`` shape (``answer`` rows + parallel
        ``claim_ids``, ``source_byte_spans``, ``source_authority``,
        ``source_blake3s`` arrays + trust/temporal/lineage/privacy
        fields + ``caveats``).
        """
        if self._engine is not None:
            return self._engine.probe_engram(
                pointer,
                question,
                clearance=clearance,
                probe_kind=probe_kind,
                score_with_hybrid=score_with_hybrid,
            )
        body = {"question": question, "score_with_hybrid": score_with_hybrid}
        if clearance is not None:
            body["clearance"] = clearance
        if probe_kind is not None:
            body["probe_kind"] = probe_kind
        return self._client_post_with_session(
            f"/ws/{self._workspace}/engrams/{pointer}/probe", body
        )

    def engrams(self) -> list[dict]:
        """List active engrams for this Brain's session."""
        if self._engine is not None:
            return self._engine.list_engrams()
        return self._client_get_with_session(
            f"/ws/{self._workspace}/engrams"
        )

    def expire(self, pointer: str) -> bool:
        """Drop an engram. Returns True when one was removed."""
        if self._engine is not None:
            return self._engine.expire_engram(pointer)
        result = self._client_delete_with_session(
            f"/ws/{self._workspace}/engrams/{pointer}"
        )
        return bool(result.get("expired", False))

    def reset_session(self) -> None:
        """Drop every engram in this Brain's session."""
        if self._engine is not None:
            self._engine.reset_session()
            return
        # Remote: there's no explicit /sessions/{id} DELETE endpoint
        # (engrams are per-session, so dropping each one accomplishes
        # the same). We list-and-evict; idempotent.
        for ref in self.engrams():
            self.expire(ref["pointer"])

    # ─── Internal HTTP helpers (session-aware) ─────────────────
    #
    # The bare `Client` doesn't know about session ids — engram
    # endpoints expect the X-TR-Session-Id header.  These helpers
    # inject it without forking the existing Client public API.

    def _client_post_with_session(self, path: str, body: dict) -> Any:
        assert self._client is not None
        resp = self._client._client.post(
            f"{self._client._base}{path}",
            json=body,
            headers={"X-TR-Session-Id": self._session_id},
        )
        return self._client._handle(resp)

    def _client_get_with_session(self, path: str) -> Any:
        assert self._client is not None
        resp = self._client._client.get(
            f"{self._client._base}{path}",
            headers={"X-TR-Session-Id": self._session_id},
        )
        return self._client._handle(resp)

    def _client_delete_with_session(self, path: str) -> Any:
        assert self._client is not None
        resp = self._client._client.delete(
            f"{self._client._base}{path}",
            headers={"X-TR-Session-Id": self._session_id},
        )
        return self._client._handle(resp)


def _resolve_first_workspace(client: Client) -> str:
    """Return the name of the first workspace mounted on ``client``.

    Raises :class:`APIError` (with ``code='NO_WORKSPACE'``) if none
    are mounted — same shape the bare ``Client._resolve_workspace``
    raises so callers don't need to handle two error styles.
    """
    workspaces = client.workspaces()
    if not workspaces:
        raise APIError(
            status_code=404,
            code="NO_WORKSPACE",
            message="No workspaces mounted on the daemon",
        )
    return workspaces[0]["name"]


def _new_session_id() -> str:
    """Mint a fresh Brain session id.  Matches the Rust pattern of
    ``py-<16 hex>`` for cross-language traceability."""
    return f"py-{secrets.token_hex(8)}"
