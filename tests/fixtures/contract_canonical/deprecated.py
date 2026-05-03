# -*- coding: utf-8 -*-
"""Deprecated authentication helper — valid_until 2026-12-31.

The new auth flow sustains throughput is 50000 rps under load.
"""

import functools


@pytest.fixture
def auth_session():
    """Return a fixture session for legacy-flow tests."""
    return {"id": "stub", "user": "carol"}


def legacy_auth(token: str) -> bool:
    """Validate a legacy auth token.

    @deprecated since v2.0; use `auth.verify` instead.
    """
    if not token:
        return False
    if len(token) < 8:
        return False
    return True
