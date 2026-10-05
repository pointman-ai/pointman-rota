"""Rota's accounts and the rule that picks one (standard library only, so `python3 -m rota` works anywhere).

- **The accounts** are this machine's own Claude login ("default", no token: a run uses whatever Claude Code
  is signed in as) and any added with `python3 -m rota add <label>`, each a setup token from
  `claude setup-token`. They're kept in ~/.config/message-board/rota.json, readable by this user only.
- **What rota knows of each** comes from Claude Code itself: every run's stream reports its account's
  5-hour and weekly windows (`rate_limit_event`), and an account nothing has run on lately is checked
  with a one-line Haiku run. That's kept in the extension's state folder, with no tokens in it.
- **The rule:** new runs stay on the account they're on until either window passes the threshold (90%),
  then move to the account with the most room left, if it's at least 10 points under the threshold.
"""

from __future__ import annotations

import json
import os
import re
import time
from pathlib import Path

DEFAULT = "default"
MARGIN = 10                 # an account to move to must be this far under the threshold: no flip-flopping
LABEL = re.compile(r"[a-z0-9][a-z0-9._-]{0,31}")
TOKEN = re.compile(r"sk-ant-oat\d+-[A-Za-z0-9_-]{20,}")


def store_path() -> Path:
    return Path(os.environ.get("ROTA_STORE") or "~/.config/message-board/rota.json").expanduser()


def load_store(path: Path | None = None) -> dict:
    try:
        d = json.loads((path or store_path()).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        d = {}
    accounts = [a for a in d.get("accounts") or [] if isinstance(a, dict) and LABEL.fullmatch(str(a.get("label")))]
    codex = [a for a in d.get("codex") or [] if isinstance(a, dict) and LABEL.fullmatch(str(a.get("label")))]
    return {"accounts": accounts, "codex": codex}


def save_store(store: dict, path: Path | None = None) -> None:
    path = path or store_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)      # tokens: this user only
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(store, f, indent=2)
    os.chmod(tmp, 0o600)
    tmp.replace(path)


def hold(label: str, held: bool, path: Path | None = None) -> None:
    """Keep runs off an account, or put it back in turn. One kept in the vault is held here by its label."""
    store = load_store(path)
    named = next((a for a in store["accounts"] if a["label"] == label), None)
    if named is None:
        named = {"label": label}
        store["accounts"].append(named)
    named["held"] = held
    save_store(store, path)


def tokens(store: dict) -> dict[str, str | None]:
    """Each usable account's token, in order: this machine's own login first (no token)."""
    out: dict[str, str | None] = {DEFAULT: None}
    for a in store["accounts"]:
        if not a.get("held") and a["label"] != DEFAULT and isinstance(a.get("token"), str):
            out[a["label"]] = a["token"]
    return out


def windows(limits: dict | None) -> dict:
    """A rate_limit_info as rota keeps it: {five_hour, seven_day: {used (%), resets (epoch)}, status}."""
    out: dict = {}
    for name, w in ((limits or {}).get("unifiedWindows") or {}).items():
        if name in ("five_hour", "seven_day") and isinstance(w, dict) and isinstance(w.get("utilization"), (int, float)):
            out[name] = {"used": round(float(w["utilization"]) * 100, 1), "resets": w.get("resetsAt")}
    if limits and limits.get("status"):
        out["status"] = limits["status"]
        if limits["status"] != "allowed" and limits.get("resetsAt"):
            out["blocked_until"] = limits["resetsAt"]
    return out


def used(seen: dict | None, now: float | None = None) -> float | None:
    """How full the account is: its fuller window, a window that has reset counting as empty, and 100
    while Claude says it's over its limit. None: nothing known yet."""
    if not seen or not any(k in seen for k in ("five_hour", "seven_day")):
        return None
    now = now if now is not None else time.time()
    if seen.get("blocked_until") and now < float(seen["blocked_until"]):
        return 100.0
    fills = [0.0 if w.get("resets") and now >= float(w["resets"]) else float(w["used"])
             for k, w in seen.items() if k in ("five_hour", "seven_day")]
    return max(fills)


def choose(labels: list[str], seen: dict[str, dict], current: str | None, threshold: float,
           now: float | None = None) -> str:
    """The account a new run starts on. `labels` are the usable ones, in order (the machine's own first)."""
    if not labels:
        return DEFAULT
    fill = {label: used(seen.get(label), now) for label in labels}
    usable = [label for label in labels if not (seen.get(label) or {}).get("needs_auth")] or labels
    if current in usable and (fill[current] is None or fill[current] < threshold):
        return current
    known = [label for label in usable if fill[label] is not None]
    best = min(known, key=lambda label: fill[label], default=None)
    if best is not None and fill[best] <= threshold - MARGIN:
        return best
    unknown = [label for label in usable if fill[label] is None]
    if unknown:                                  # nothing known yet beats one known to be full
        return unknown[0]
    if current in usable:
        return current
    return best or usable[0]


def most_room(labels: list[str], seen: dict[str, dict], now: float | None = None) -> str | None:
    """The account with the most room left, of those not refused: one never checked counts as empty."""
    usable = [label for label in labels if not (seen.get(label) or {}).get("needs_auth")]
    return min(usable, key=lambda label: used(seen.get(label), now) or 0.0, default=None)


def describe(label: str, seen: dict | None, current: str | None, now: float | None = None) -> str:
    """'5h 46% (resets 14:30) · week 37% (resets Tue) · runs start here', for health and status."""
    now = now if now is not None else time.time()
    bits = []
    for key, name in (("five_hour", "5h"), ("seven_day", "week")):
        w = (seen or {}).get(key)
        if not w:
            continue
        resets = w.get("resets")
        if resets and now >= float(resets):
            bits.append(f"{name} reset")
            continue
        when = ""
        if resets:
            t = time.localtime(float(resets))
            when = time.strftime("%H:%M" if float(resets) - now < 20 * 3600 else "%a %H:%M", t)
        bits.append(f"{name} {w['used']:.0f}%" + (f" (resets {when})" if when else ""))
    if not bits:
        bits.append("not checked yet")
    if (seen or {}).get("needs_auth"):
        bits.append("its token was refused")
    if label == current:
        bits.append("runs start here")
    return " · ".join(bits)
