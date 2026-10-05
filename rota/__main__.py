"""`python -m rota`: with no arguments, the extension its node runs; with one, the command line for adding
this machine's accounts (`python3 -m rota add work`), which only this machine's user can do."""

from __future__ import annotations

import sys

if len(sys.argv) > 1:
    from rota.cli import main
    sys.exit(main(sys.argv[1:]))

import json
import threading
import time
from pathlib import Path

from mb_extension import Extension

from rota import accounts, probe

ext = Extension()
lock = threading.Lock()
state: dict = {"seen": {}, "current": None}
LIST_SECONDS = 60           # how often the vault's rota.* names are looked at
OPEN_SECONDS = 1800         # how long an opened token is kept before it's opened again (each open is logged)
vault: dict = {"listed": 0.0, "names": [], "opened": {}, "said": None}
vault_lock = threading.Lock()


def vault_tokens() -> dict[str, str]:
    """The accounts kept in the board's vault as `rota.<label>`, for this machine: their names listed
    and each token opened through the node, which only lets rota see rota.* (MB-106)."""
    now = time.time()
    with vault_lock:
        if now - vault["listed"] > LIST_SECONDS:
            vault["listed"] = now
            try:
                vault["names"] = ext.request("secret.list", {"prefix": "rota."})["names"]
                vault["said"] = None
            except RuntimeError as e:          # an older node or core: the local file only
                if vault["said"] != str(e):
                    ext.log(f"no accounts from the vault: {e}")
                    vault["said"] = str(e)
        names = list(vault["names"])
    out = {}
    for name in names:
        label = name[len("rota."):].lower()
        if label == accounts.DEFAULT or not accounts.LABEL.fullmatch(label):
            continue
        kept = vault["opened"].get(name)
        if kept is None or now - kept[0] > OPEN_SECONDS:
            try:
                found = accounts.TOKEN.search(ext.secret(name))
            except RuntimeError as e:
                ext.log(f"couldn't open {name}: {e}")
                continue
            kept = vault["opened"][name] = (now, found[0] if found else None)
        if kept[1]:
            out[label] = kept[1]
    return out


def all_tokens() -> dict[str, str | None]:
    """Every account runs may start on: this machine's own login, the vault's, and any in the local
    file (the vault's win for the same label); none that are held."""
    store = accounts.load_store()
    out = accounts.tokens(store)
    held = {a["label"] for a in store["accounts"] if a.get("held")}
    out.update({label: token for label, token in vault_tokens().items() if label not in held})
    return out


def state_file() -> Path:
    return Path(ext.node.get("state_dir") or ".") / "rota.json"


def load_state() -> None:
    try:
        d = json.loads(state_file().read_text(encoding="utf-8"))
        state.update({"seen": d.get("seen") or {}, "current": d.get("current")})
    except (OSError, ValueError):
        pass


def save_state() -> None:
    path = state_file()
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(state), encoding="utf-8")
    tmp.replace(path)


def threshold() -> float:
    return float(ext.settings.get("threshold") or 90)


def summary() -> dict:
    """What the apps and agents see: labels, how full, when each resets. Never a token."""
    toks = all_tokens()
    now = time.time()
    return {"current": state["current"], "threshold": threshold(), "accounts": [
        {"label": label, "own_login": label == accounts.DEFAULT, "used": accounts.used(state["seen"].get(label), now),
         **{k: v for k, v in (state["seen"].get(label) or {}).items() if k in ("five_hour", "seven_day", "status",
                                                                              "needs_auth", "checked_at")},
         "line": accounts.describe(label, state["seen"].get(label), state["current"], now)}
        for label in toks]}


def note(label: str, limits: dict | None, error: str | None, refused: bool) -> None:
    with lock:
        seen = state["seen"].setdefault(label, {})
        if limits:
            for k in ("five_hour", "seven_day", "status", "blocked_until"):
                seen.pop(k, None)
            seen.update(accounts.windows(limits))
            seen["needs_auth"] = False
        elif refused:
            seen["needs_auth"] = True
        seen["checked_at"] = time.time()
        seen["error"] = error
        save_state()
    ext.event("rota.accounts", summary())


@ext.on_start
def started() -> None:
    load_state()
    ext.event("rota.accounts", summary())


@ext.method("session.prepare")
def prepare(p: dict) -> dict:
    """The account a run starts on: its token goes in the run's environment, and none for this
    machine's own login."""
    if p.get("client") != "claude":
        return {}
    toks = all_tokens()
    with lock:
        label = accounts.choose(list(toks), state["seen"], state["current"], threshold())
        if label != state["current"]:
            ext.log(f"runs start on {label} now (was {state['current']})")
            state["current"] = label
            save_state()
    return {"env": {"CLAUDE_CODE_OAUTH_TOKEN": toks[label]} if toks[label] else {}, "account": label}


@ext.method("session.ended")
def ended(p: dict) -> dict:
    """A run's account as Claude Code last reported it, and whether its token was refused."""
    label = p.get("account") or accounts.DEFAULT
    error = p.get("error")
    note(label, p.get("limits"), error, probe.refused(error) and not p.get("limits"))
    return {}


@ext.method("health.check")
def health(p: dict) -> list[dict]:
    """Each account as a part under rota in `mb health` and the apps' Machines page."""
    out = []
    now = time.time()
    for label in all_tokens():
        seen = state["seen"].get(label) or {}
        full = accounts.used(seen, now)
        refused = bool(seen.get("needs_auth"))
        out.append({"id": label, "kind": "account", "name": f"Claude: {label}",
                    "state": "failing" if refused or (full is not None and full >= 100) else
                    "unknown" if full is None else "ok",
                    "needs": ["auth"] if refused else [],
                    "why": accounts.describe(label, seen, state["current"], now)})
    return out


@ext.method("health.fix")
def fix(p: dict) -> dict:
    label = p.get("part")
    if p.get("action") != "auth" or label == accounts.DEFAULT:
        raise ValueError("rota can only say how to sign an added account in again")
    return {"message": f"On {ext.node.get('machine')}: run `claude setup-token` signed in as {label}'s account, "
                       f"then `python3 -m rota add {label}` in the mb-rota folder and paste the token"}


@ext.command("rota.status")
def status(job) -> dict:
    return summary()


def check_idle() -> None:
    """Check each account nothing has told rota about lately, one at a time."""
    toks = all_tokens()
    every = 60 * float(ext.settings.get("check_minutes") or 30)
    for label, token in toks.items():
        seen = state["seen"].get(label) or {}
        if time.time() - float(seen.get("checked_at") or 0) < every:
            continue
        got = probe.check(token)
        note(label, got["limits"], got["error"], got["refused"])


ext.every(60, check_idle, lane="checks")
ext.run()
