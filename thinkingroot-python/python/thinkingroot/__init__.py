"""ThinkingRoot — Knowledge compiler for AI agents.

The recommended entry point is the :class:`Brain` facade, which
abstracts over in-process (PyO3) and remote (HTTP) transports::

    from thinkingroot import Brain

    brain = Brain.connect()           # cortex-aware
    # or
    brain = Brain.open("./project")   # in-process
    # or
    brain = Brain.remote("http://localhost:31760")
    # or
    brain = Brain.mount("./pack.tr")  # spawn root mount → attach

    pointer = brain.materialize_engram("auth flow")["pointer"]
    answer = brain.probe(pointer, "what changed?")

The lower-level surfaces remain available for advanced use:

* :func:`compile`, :func:`parse_directory`, :func:`parse_file` —
  PyO3-bound pipeline entry points (require the native extension).
* :func:`open`, :class:`Engine` — direct PyO3 engine handle.
* :class:`Client` — bare httpx wrapper around the REST API.
* :mod:`thinkingroot.cortex` — cortex.lock discovery primitives.

See ``docs/secondary-brain-concept.md`` for the architecture story.
"""

from thinkingroot._thinkingroot import (
    Engine,
    ThinkingRootError,
    compile,
    open,
    parse_directory,
    parse_file,
)
from thinkingroot.brain import Brain, BrainInfo
from thinkingroot.client import APIError, Client
from thinkingroot.cortex import CortexError, CortexLock, IncompatibleLockSchema

__all__ = [
    # High-level facade
    "Brain",
    "BrainInfo",
    # Low-level transports
    "Client",
    "Engine",
    "APIError",
    "ThinkingRootError",
    # Cortex discovery
    "CortexLock",
    "CortexError",
    "IncompatibleLockSchema",
    # Pipeline entry points
    "compile",
    "open",
    "parse_directory",
    "parse_file",
]
