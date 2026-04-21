"""Phase F regression tests: the seven SDK branch methods must dispatch to
the correct REST paths with the expected HTTP verbs and payloads.

Uses an in-process stub transport so no live server is required.
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path
from typing import Any

import httpx
import pytest

# Load `thinkingroot.client` directly from its file path — the package's
# __init__.py depends on native Rust bindings that aren't built in CI-lite.
_SPEC = importlib.util.spec_from_file_location(
    "thinkingroot_client",
    Path(__file__).resolve().parent.parent / "python" / "thinkingroot" / "client.py",
)
_MOD = importlib.util.module_from_spec(_SPEC)
sys.modules["thinkingroot_client"] = _MOD
_SPEC.loader.exec_module(_MOD)
Client = _MOD.Client


def _ok(data: Any) -> httpx.Response:
    return httpx.Response(200, content=json.dumps({"ok": True, "data": data}).encode())


class _Recorder:
    """Captures the last request and returns canned `ok_response` payloads."""

    def __init__(self) -> None:
        self.last: httpx.Request | None = None

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.last = request
        path = request.url.path
        method = request.method
        # Return a plausible shape per endpoint so SDK unwrapping succeeds.
        if method == "GET" and path == "/api/v1/branches":
            return _ok({"branches": [{"name": "main", "status": "Active"}]})
        if method == "POST" and path == "/api/v1/branches":
            return _ok({"branch": {"name": "feat", "parent": "main"}})
        if path.endswith("/diff"):
            return _ok({"new_claims": []})
        if path.endswith("/merge"):
            return _ok({"merged": "feat"})
        if path.endswith("/checkout"):
            return _ok({"head": "feat"})
        if path.endswith("/rollback"):
            return _ok({"rolled_back": "feat"})
        if method == "DELETE":
            return _ok({"deleted": "feat"})
        return _ok({})


def _make_client() -> tuple[Client, _Recorder]:
    recorder = _Recorder()
    transport = httpx.MockTransport(recorder)
    c = Client(base_url="http://test")
    c._client = httpx.Client(base_url="http://test", transport=transport)
    return c, recorder


def test_branches_get_path():
    c, rec = _make_client()
    got = c.branches()
    assert rec.last is not None and rec.last.method == "GET"
    assert rec.last.url.path == "/api/v1/branches"
    assert isinstance(got, list)


def test_create_branch_posts_json_body():
    c, rec = _make_client()
    c.create_branch("feat", description="X")
    assert rec.last.method == "POST"
    assert rec.last.url.path == "/api/v1/branches"
    body = json.loads(rec.last.content.decode())
    assert body == {"name": "feat", "parent": "main", "description": "X"}


def test_create_branch_omits_description_when_none():
    c, rec = _make_client()
    c.create_branch("feat")
    body = json.loads(rec.last.content.decode())
    assert body == {"name": "feat", "parent": "main"}


def test_diff_branch_get():
    c, rec = _make_client()
    c.diff_branch("feat")
    assert rec.last.method == "GET"
    assert rec.last.url.path == "/api/v1/branches/feat/diff"


def test_merge_branch_posts_flags():
    c, rec = _make_client()
    c.merge_branch("feat", force=True)
    assert rec.last.method == "POST"
    assert rec.last.url.path == "/api/v1/branches/feat/merge"
    body = json.loads(rec.last.content.decode())
    assert body == {"force": True, "propagate_deletions": False}


def test_checkout_branch_post():
    c, rec = _make_client()
    c.checkout_branch("feat")
    assert rec.last.method == "POST"
    assert rec.last.url.path == "/api/v1/branches/feat/checkout"


def test_delete_branch_uses_http_delete():
    c, rec = _make_client()
    c.delete_branch("feat")
    assert rec.last.method == "DELETE"
    assert rec.last.url.path == "/api/v1/branches/feat"


def test_rollback_merge_post():
    c, rec = _make_client()
    c.rollback_merge("feat")
    assert rec.last.method == "POST"
    assert rec.last.url.path == "/api/v1/branches/feat/rollback"
