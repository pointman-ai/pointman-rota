"""Codex accounts (MB-45): each one is a Codex home of its own, signed in once with `codex login`.

- **The accounts:** this machine's own Codex login (`default`, ~/.codex, or $CODEX_HOME) and any added
  with `python3 -m rota codex add <label>`, each a folder under ~/.config/message-board/rota/codex/,
  readable by this user only. Codex keeps the account's sign-in there (auth.json) and refreshes it
  itself, so its tokens never leave that folder. The machine's own config.toml is linked in, so runs on
  any account have the same settings and servers.
- **A run's account:** the node asks rota before each Codex run (`session.prepare`), and the run gets
  `CODEX_HOME` set to that account's folder (nothing for the machine's own).
- **What rota knows of each** comes from Codex itself: every session it writes (sessions/…/rollout-*.jsonl
  in the account's home) has the account's windows in its `rate_limits` (`used_percent`,
  `window_minutes`, `resets_at`). Rota reads the newest after each run. Codex isn't probed: an account
  nothing has run on stays unknown, which the rule prefers to a full one.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

from rota import accounts

WINDOWS = {300: "five_hour", 10080: "seven_day"}


def own_home() -> Path:
    return Path(os.environ.get("CODEX_HOME") or "~/.codex").expanduser()


def homes_dir() -> Path:
    return Path(os.environ.get("ROTA_CODEX_HOMES") or "~/.config/message-board/rota/codex").expanduser()


def home(label: str) -> Path:
    return own_home() if label == accounts.DEFAULT else homes_dir() / label


def in_use(store: dict) -> bool:
    """Whether this machine has Codex: installed, or an account added."""
    return bool(shutil.which("codex") or store.get("codex") or (own_home() / "auth.json").exists())


def labels(store: dict) -> list[str]:
    """Each usable Codex account, the machine's own first: the added ones signed in and not held. None
    on a machine without Codex."""
    if not in_use(store):
        return []
    out = [accounts.DEFAULT]
    for a in store.get("codex") or []:
        label = a.get("label")
        if isinstance(label, str) and accounts.LABEL.fullmatch(label) and label != accounts.DEFAULT \
                and not a.get("held") and (home(label) / "auth.json").exists():
            out.append(label)
    return out


def windows(rate_limits: dict | None) -> dict:
    """Codex's rate_limits as rota keeps an account's: {five_hour, seven_day: {used, resets}}, by each
    window's length (5 h, a week)."""
    out: dict = {}
    for key in ("primary", "secondary"):
        w = (rate_limits or {}).get(key)
        if not isinstance(w, dict) or not isinstance(w.get("used_percent"), (int, float)):
            continue
        name = WINDOWS.get(int(w.get("window_minutes") or 0))
        if name:
            out[name] = {"used": round(float(w["used_percent"]), 1), "resets": w.get("resets_at")}
    return out


def latest(label: str) -> dict | None:
    """The account's windows as its newest Codex session last reported them, or None."""
    sessions = home(label) / "sessions"
    try:
        newest = max(sessions.rglob("rollout-*.jsonl"), key=lambda p: p.stat().st_mtime)
    except (ValueError, OSError):
        return None
    found = None
    try:
        with open(newest, encoding="utf-8", errors="ignore") as f:
            for line in f:
                if '"rate_limits"' in line:
                    found = line
        d = json.loads(found) if found else None
    except (OSError, ValueError):
        return None
    payload = (d or {}).get("payload") or (d or {}).get("msg") or d or {}
    got = windows(payload.get("rate_limits") if isinstance(payload, dict) else None)
    return got or None


def add(label: str, store_path: Path | None = None, *, codex: str | None = None, run=subprocess.run) -> str:
    """`python3 -m rota codex add <label>`: a Codex home for the account, signed in with `codex login`
    (Codex opens the browser), the machine's config linked in, and the label kept."""
    codex = codex or shutil.which("codex") or "/opt/homebrew/bin/codex"
    where = home(label)
    where.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(where, 0o700)
    mine = own_home() / "config.toml"
    if mine.exists() and not (where / "config.toml").exists():
        (where / "config.toml").symlink_to(mine)
    r = run([codex, "login"], env={**os.environ, "CODEX_HOME": str(where)})
    if getattr(r, "returncode", 1) != 0 or not (where / "auth.json").exists():
        raise RuntimeError(f"codex login didn't finish: {label} isn't signed in ({where})")
    store = accounts.load_store(store_path)
    store["codex"] = [a for a in store.get("codex") or [] if a.get("label") != label] + [{"label": label}]
    accounts.save_store(store, store_path)
    return f"added Codex account {label} ({where}). New Codex runs can start on it from now on."
