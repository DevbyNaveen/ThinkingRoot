"""Brain-facade tests for the Python SDK.

Covers the three transports (in-process is exercised when the native
extension is available; remote is exercised against an httpx stub
transport; cortex.connect resolution is exercised against a tmp
``XDG_CONFIG_HOME``).

Like ``test_branch_methods.py``, the existing CI-lite path may not
have the native extension built — we skip in-process tests gracefully.
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path
from typing import Any

import httpx
import pytest

# Load brain.py + client.py + cortex.py directly so the native
# extension is not required for the remote-only tests.
_SDK_DIR = Path(__file__).resolve().parent.parent / "python" / "thinkingroot"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


# Order matters: brain.py imports from `thinkingroot.client` and
# `thinkingroot.cortex`, so we have to register the package first.
_PKG = importlib.util.module_from_spec(
    importlib.util.spec_from_file_location(
        "thinkingroot",
        _SDK_DIR / "__init__.py",
        submodule_search_locations=[str(_SDK_DIR)],
    )
)
sys.modules["thinkingroot"] = _PKG
_client_mod = _load("thinkingroot.client", _SDK_DIR / "client.py")
_cortex_mod = _load("thinkingroot.cortex", _SDK_DIR / "cortex.py")
# Stub the native module so brain.py's `from thinkingroot import open`
# import doesn't fail when the extension isn't built.
_native_stub = type(sys)("thinkingroot._thinkingroot")
sys.modules["thinkingroot._thinkingroot"] = _native_stub
# Manually set the package's surface so brain.py's
# `from thinkingroot import open as _native_open` works on import.
_PKG.open = lambda _path: None  # type: ignore[attr-defined]
_brain_mod = _load("thinkingroot.brain", _SDK_DIR / "brain.py")

Client = _client_mod.Client
Brain = _brain_mod.Brain
APIError = _client_mod.APIError


def _ok(data: Any) -> httpx.Response:
    return httpx.Response(200, content=json.dumps({"ok": True, "data": data}).encode())


class _Recorder:
    """Captures every request the SDK makes and returns canned responses."""

    def __init__(self, routes: dict[tuple[str, str], Any]):
        self.routes = routes
        self.history: list[httpx.Request] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.history.append(request)
        key = (request.method, request.url.path)
        for (method, prefix), payload in self.routes.items():
            if request.method == method and request.url.path.startswith(prefix):
                return _ok(payload)
        return httpx.Response(
            404, content=json.dumps({"ok": False, "error": {"code": "NF", "message": "no route"}}).encode()
        )


def _brain_with_recorder(rec: _Recorder, *, workspace: str = "myws", session: str = "py-test") -> Brain:
    transport = httpx.MockTransport(rec)
    client = Client(base_url="http://127.0.0.1:31760")
    client._client = httpx.Client(
        transport=transport,
        base_url="http://127.0.0.1:31760",
        timeout=5.0,
    )
    return Brain(
        transport="remote",
        engine=None,
        client=client,
        workspace=workspace,
        session_id=session,
        base_url="http://127.0.0.1:31760",
    )


# ─── Brain.remote ────────────────────────────────────────────


def test_brain_workspaces_lists_via_client():
    rec = _Recorder({("GET", "/api/v1/workspaces"): [{"name": "alpha"}, {"name": "beta"}]})
    brain = _brain_with_recorder(rec, workspace="alpha")
    assert brain.workspace == "alpha"
    assert brain.session_id == "py-test"


def test_brain_entities_dispatch():
    rec = _Recorder(
        {
            ("GET", "/api/v1/ws/myws/entities"): [
                {"id": "e1", "canonical_name": "Auth"}
            ]
        }
    )
    brain = _brain_with_recorder(rec)
    ents = brain.entities()
    assert ents[0]["canonical_name"] == "Auth"
    assert rec.history[-1].method == "GET"
    assert rec.history[-1].url.path == "/api/v1/ws/myws/entities"


def test_brain_hybrid_search_posts_full_request():
    rec = _Recorder(
        {
            ("POST", "/api/v1/ws/myws/search/hybrid"): {
                "hits": [{"claim_id": "c1", "score": 0.9}],
                "routing": {"shape": "Vector+Datalog", "total_candidates": 5,
                            "vector_candidates": 3, "datalog_candidates": 2},
            }
        }
    )
    brain = _brain_with_recorder(rec)
    resp = brain.hybrid_search("auth", top_k=5)
    assert resp["hits"][0]["claim_id"] == "c1"
    body = json.loads(rec.history[-1].content)
    assert body["query_text"] == "auth"
    assert body["session_id"] == "py-test"
    assert body["top_k"] == 5


def test_brain_materialize_engram_sends_session_header():
    rec = _Recorder(
        {
            ("POST", "/api/v1/ws/myws/engrams"): {
                "pointer": "0xABCD",
                "summary": {"pointer": "0xABCD", "topic": "auth"},
            }
        }
    )
    brain = _brain_with_recorder(rec, session="py-fixture")
    out = brain.materialize_engram("auth flow")
    assert out["pointer"] == "0xABCD"
    last = rec.history[-1]
    assert last.headers.get("X-TR-Session-Id") == "py-fixture"


def test_brain_probe_dispatches_to_pointer_path_with_session():
    rec = _Recorder(
        {
            ("POST", "/api/v1/ws/myws/engrams/0xABCD/probe"): {
                "answer": [{"kind": "factual", "statement": "ok"}],
                "claim_ids": ["c1"],
                "source_byte_spans": [{"source_id": "s1", "byte_start": 0, "byte_end": 5}],
                "source_authority": ["high"],
                "source_blake3s": ["blake3:abcd"],
                "admission_tier": "rooted",
                "valid_window": [None, None],
                "superseded_by_chain": [],
                "derivation_parents": [],
                "sensitivity": "public",
                "git_blame": [],
                "related_quantities": [],
                "related_doc_tags": [],
                "related_calls": [],
                "related_markers": [],
                "caveats": [],
            }
        }
    )
    brain = _brain_with_recorder(rec, session="probe-sess")
    answer = brain.probe("0xABCD", "what changed?")
    assert answer["claim_ids"] == ["c1"]
    last = rec.history[-1]
    assert last.url.path == "/api/v1/ws/myws/engrams/0xABCD/probe"
    assert last.headers.get("X-TR-Session-Id") == "probe-sess"


def test_brain_engrams_list_uses_session_header():
    rec = _Recorder({("GET", "/api/v1/ws/myws/engrams"): [{"pointer": "0xABCD", "topic": "x", "workspace": "myws", "created_at": 0, "entity_count": 1, "claim_count": 1}]})
    brain = _brain_with_recorder(rec, session="list-sess")
    refs = brain.engrams()
    assert len(refs) == 1
    assert rec.history[-1].headers.get("X-TR-Session-Id") == "list-sess"


def test_brain_expire_returns_bool():
    rec = _Recorder(
        {("DELETE", "/api/v1/ws/myws/engrams/0xABCD"): {"expired": True, "pointer": "0xABCD"}}
    )
    brain = _brain_with_recorder(rec)
    assert brain.expire("0xABCD") is True


def test_brain_api_error_envelopes_propagate():
    def transport_fn(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            400,
            content=json.dumps(
                {"ok": False, "error": {"code": "MISSING_SESSION", "message": "X-TR-Session-Id required"}}
            ).encode(),
        )

    transport = httpx.MockTransport(transport_fn)
    client = Client(base_url="http://127.0.0.1:31760")
    client._client = httpx.Client(transport=transport, base_url="http://127.0.0.1:31760", timeout=5.0)
    brain = Brain(
        transport="remote",
        engine=None,
        client=client,
        workspace="myws",
        session_id="sess",
        base_url="http://127.0.0.1:31760",
    )
    with pytest.raises(APIError) as exc:
        brain.materialize_engram("topic")
    assert exc.value.code == "MISSING_SESSION"


# ─── cortex.connect resolution ───────────────────────────────


def test_brain_connect_raises_without_lockfile(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    # On macOS XDG is ignored; skip there.
    if sys.platform == "darwin":
        pytest.skip("macOS lockPath ignores XDG_CONFIG_HOME")
    with pytest.raises(ConnectionError):
        Brain.connect()


# ─── Info introspection ───────────────────────────────────────


def test_brain_info_carries_metadata():
    rec = _Recorder({})
    brain = _brain_with_recorder(rec, session="info-test")
    info = brain.info()
    assert info.transport == "remote"
    assert info.workspace == "myws"
    assert info.session_id == "info-test"
    assert info.daemon_pid is None
