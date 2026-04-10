"""ThinkingRoot — Knowledge compiler for AI agents."""

from thinkingroot._thinkingroot import compile, parse_directory, parse_file, open, Engine, ThinkingRootError

try:
    from thinkingroot.client import Client
except ImportError:
    pass  # httpx not installed — native bindings still work

__all__ = ["compile", "parse_directory", "parse_file", "open", "Engine", "ThinkingRootError", "Client"]
