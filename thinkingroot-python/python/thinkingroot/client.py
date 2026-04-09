"""ThinkingRoot HTTP client for querying a running server."""

from __future__ import annotations

from typing import Any

import httpx


class APIError(Exception):
    """Error returned by the ThinkingRoot REST API."""

    def __init__(self, status_code: int, code: str, message: str):
        self.status_code = status_code
        self.code = code
        self.message = message
        super().__init__(f"[{status_code}] {code}: {message}")


class Client:
    """HTTP client for ThinkingRoot REST API.

    Usage:
        client = Client("http://localhost:3000", api_key="optional")
        entities = client.entities(workspace="my-repo")
    """

    def __init__(
        self,
        base_url: str = "http://localhost:3000",
        api_key: str | None = None,
    ):
        headers = {}
        if api_key:
            headers["Authorization"] = f"Bearer {api_key}"
        self._client = httpx.Client(base_url=base_url, headers=headers, timeout=120.0)
        self._base = "/api/v1"

    def _get(self, path: str, params: dict[str, Any] | None = None) -> Any:
        resp = self._client.get(f"{self._base}{path}", params=params)
        return self._handle(resp)

    def _post(self, path: str) -> Any:
        resp = self._client.post(f"{self._base}{path}")
        return self._handle(resp)

    def _handle(self, resp: httpx.Response) -> Any:
        data = resp.json()
        if not data.get("ok"):
            error = data.get("error", {})
            raise APIError(
                status_code=resp.status_code,
                code=error.get("code", "UNKNOWN"),
                message=error.get("message", "Unknown error"),
            )
        return data.get("data")

    # ─── Workspace ────────────────────────────────────────

    def workspaces(self) -> list[dict[str, Any]]:
        return self._get("/workspaces")

    # ─── Entities ─────────────────────────────────────────

    def entities(self, workspace: str) -> list[dict[str, Any]]:
        return self._get(f"/ws/{workspace}/entities")

    def entity(self, name: str, workspace: str) -> dict[str, Any]:
        return self._get(f"/ws/{workspace}/entities/{name}")

    # ─── Claims ───────────────────────────────────────────

    def claims(
        self,
        workspace: str,
        type: str | None = None,
        entity: str | None = None,
        min_confidence: float | None = None,
        limit: int | None = None,
        offset: int | None = None,
    ) -> list[dict[str, Any]]:
        params: dict[str, Any] = {}
        if type:
            params["type"] = type
        if entity:
            params["entity"] = entity
        if min_confidence is not None:
            params["min_confidence"] = min_confidence
        if limit is not None:
            params["limit"] = limit
        if offset is not None:
            params["offset"] = offset
        return self._get(f"/ws/{workspace}/claims", params=params)

    # ─── Relations ────────────────────────────────────────

    def relations(self, entity: str, workspace: str) -> list[dict[str, Any]]:
        return self._get(f"/ws/{workspace}/relations/{entity}")

    def all_relations(self, workspace: str) -> list[dict[str, Any]]:
        return self._get(f"/ws/{workspace}/relations")

    # ─── Artifacts ────────────────────────────────────────

    def artifacts(self, workspace: str) -> list[dict[str, Any]]:
        return self._get(f"/ws/{workspace}/artifacts")

    def artifact(self, artifact_type: str, workspace: str) -> dict[str, Any]:
        return self._get(f"/ws/{workspace}/artifacts/{artifact_type}")

    # ─── Health ───────────────────────────────────────────

    def health(self, workspace: str) -> dict[str, Any]:
        return self._get(f"/ws/{workspace}/health")

    # ─── Search ───────────────────────────────────────────

    def search(
        self,
        query: str,
        workspace: str,
        top_k: int = 10,
    ) -> dict[str, Any]:
        return self._get(
            f"/ws/{workspace}/search",
            params={"q": query, "top_k": top_k},
        )

    # ─── Actions ──────────────────────────────────────────

    def compile(self, workspace: str) -> dict[str, Any]:
        return self._post(f"/ws/{workspace}/compile")

    def verify(self, workspace: str) -> dict[str, Any]:
        return self._post(f"/ws/{workspace}/verify")
