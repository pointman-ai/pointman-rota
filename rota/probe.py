"""Checking an account nothing has run on lately: a one-line Haiku run through Claude Code itself, whose
stream reports the account's limits like any run's (`rate_limit_event`). Claude Code does the talking, on
the account's own token, so rota never calls Anthropic's API directly."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

AUTH = ("oauth token", "401", "authentication", "invalid api key", "not logged in", "login")


def find_claude() -> str | None:
    for c in (shutil.which("claude"), "/opt/homebrew/bin/claude", str(Path.home() / ".local/bin/claude"),
              "/usr/local/bin/claude", str(Path.home() / ".claude/local/claude")):
        if c and Path(c).exists():
            return c
    return None


def refused(text: str | None) -> bool:
    """Whether an error says the account's token was refused (it needs signing in again)."""
    low = (text or "").lower()
    return any(word in low for word in AUTH)


def check(token: str | None, *, claude: str | None = None, timeout: float = 120) -> dict:
    """{limits, error, refused}: the account's last rate_limit_info, or why there's none."""
    claude = claude or find_claude()
    if not claude:
        return {"limits": None, "error": "Claude Code isn't installed here", "refused": False}
    env = {k: v for k, v in os.environ.items() if k not in ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN",
                                                            "CLAUDE_CODE_OAUTH_TOKEN")}
    if token:
        env["CLAUDE_CODE_OAUTH_TOKEN"] = token
    # its own empty folder, no MCP servers, no settings and nothing kept: nothing but the one turn
    folder = tempfile.mkdtemp(prefix="rota-check-")
    argv = [claude, "-p", "Reply with OK.", "--model", "haiku", "--output-format", "stream-json", "--verbose",
            "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--setting-sources", "",
            "--no-session-persistence", "--max-turns", "1"]
    try:
        r = subprocess.run(argv, cwd=folder, env=env, capture_output=True, text=True, timeout=timeout,
                           stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired:
        return {"limits": None, "error": f"no answer in {timeout:.0f} s", "refused": False}
    finally:
        shutil.rmtree(folder, ignore_errors=True)
    limits, error = None, None
    for line in r.stdout.splitlines():
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if d.get("type") == "rate_limit_event" and isinstance(d.get("rate_limit_info"), dict):
            limits = d["rate_limit_info"]
        elif d.get("type") == "result" and d.get("is_error"):
            error = str(d.get("result") or "failed")[:300]
    if limits is None and error is None and r.returncode != 0:
        error = (r.stderr.strip().splitlines() or [f"exited with {r.returncode}"])[-1][:300]
    return {"limits": limits, "error": error, "refused": refused(error) and limits is None}
