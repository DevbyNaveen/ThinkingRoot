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

        # With an explicit workspace:
        entities = client.entities(workspace="my-repo")

        # When only one workspace is mounted, workspace can be omitted:
        entities = client.entities()
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

    # ─── Internal helpers ─────────────────────────────────────

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

    def _resolve_workspace(self, workspace: str | None) -> str:
        """Return *workspace* if given, otherwise fetch and return the first mounted workspace.

        Raises ``APIError`` if no workspaces are mounted.
        """
        if workspace is not None:
            return workspace
        ws_list = self.workspaces()
        if not ws_list:
            raise APIError(
                status_code=404,
                code="NO_WORKSPACE",
                message="No workspaces mounted on the server",
            )
        return ws_list[0]["name"]

    # ─── Workspace ────────────────────────────────────────────

    def workspaces(self) -> list[dict[str, Any]]:
        """List all mounted workspaces."""
        return self._get("/workspaces")

    # ─── Entities ─────────────────────────────────────────────

    def entities(self, workspace: str | None = None) -> list[dict[str, Any]]:
        """List all entities in *workspace* (defaults to first mounted workspace)."""
        ws = self._resolve_workspace(workspace)
        return self._get(f"/ws/{ws}/entities")

    def entity(self, name: str, workspace: str | None = None) -> dict[str, Any]:
        """Get a single entity by name (case-insensitive)."""
        ws = self._resolve_workspace(workspace)
        return self._get(f"/ws/{ws}/entities/{name}")

    # ─── Claims ───────────────────────────────────────────────

    def claims(
        self,
        workspace: str | None = None,
        type: str | None = None,  # noqa: A002
        entity: str | None = None,
        min_confidence: float | None = None,
        limit: int | None = None,
        offset: int | None = None,
    ) -> list[dict[str, Any]]:
        """List claims with optional filtering."""
        ws = self._resolve_workspace(workspace)
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
        return self._get(f"/ws/{ws}/claims", params=params)

    # ─── Relations ────────────────────────────────────────────

    def relations(self, entity: str, workspace: str | None = None) -> list[dict[str, Any]]:
        """Get outgoing relations for a named entity."""
        ws = self._resolve_workspace(workspace)
        return self._get(f"/ws/{ws}/relations/{entity}")

    def all_relations(self, workspace: str | None = None) -> list[dict[str, Any]]:
        """Get all entity relations in the workspace."""
        ws = self._resolve_workspace(workspace)
        return self._get(f"/ws/{ws}/relations")

    # ─── Artifacts ────────────────────────────────────────────

    def artifacts(self, workspace: str | None = None) -> list[dict[str, Any]]:
        """List all artifact types and their availability."""
        ws = self._resolve_workspace(workspace)
        return self._get(f"/ws/{ws}/artifacts")

    def artifact(self, artifact_type: str, workspace: str | None = None) -> dict[str, Any]:
        """Fetch the content of a specific artifact."""
        ws = self._resolve_workspace(workspace)
        return self._get(f"/ws/{ws}/artifacts/{artifact_type}")

    # ─── Health ───────────────────────────────────────────────

    def health(self, workspace: str | None = None) -> dict[str, Any]:
        """Run health/verification checks on the workspace."""
        ws = self._resolve_workspace(workspace)
        return self._get(f"/ws/{ws}/health")

    # ─── Search ───────────────────────────────────────────────

    def search(
        self,
        query: str,
        workspace: str | None = None,
        top_k: int = 10,
    ) -> dict[str, Any]:
        """Semantic + keyword search across entities and claims."""
        ws = self._resolve_workspace(workspace)
        return self._get(
            f"/ws/{ws}/search",
            params={"q": query, "top_k": top_k},
        )

    # ─── Actions ──────────────────────────────────────────────

    def compile(self, workspace: str | None = None) -> dict[str, Any]:
        """Trigger a compile run (requires server-side implementation)."""
        ws = self._resolve_workspace(workspace)
        return self._post(f"/ws/{ws}/compile")

    def verify(self, workspace: str | None = None) -> dict[str, Any]:
        """Run verification checks and return the health report."""
        ws = self._resolve_workspace(workspace)
        return self._post(f"/ws/{ws}/verify")
