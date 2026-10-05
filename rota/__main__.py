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

from rota import accounts, codex, probe

ext = Extension()
lock = threading.Lock()
state: dict = {"seen": {}, "current": None, "folders": {}, "agents": {}, "left": {}, "moves": [],
               "codex": {"seen": {}, "current": None}}
MOVES_KEPT = 10             # the last swaps, for the machine's status
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
        state.update({"seen": d.get("seen") or {}, "current": d.get("current"), "folders": d.get("folders") or {},
                      "agents": d.get("agents") or {}, "left": d.get("left") or {},
                      "moves": d.get("moves") or [], "codex": d.get("codex") or {"seen": {}, "current": None}})
    except (OSError, ValueError):
        pass


def save_state() -> None:
    path = state_file()
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(state), encoding="utf-8")
    tmp.replace(path)


def threshold() -> float:
    return float(ext.settings.get("threshold") or 90)


def cooldown() -> float:
    """Seconds an account runs moved off stays out of the running (cswap's cooldown): its numbers can lag
    behind, and two nearly full accounts would otherwise take turns run by run."""
    return 60 * float(ext.settings.get("cooldown_minutes") if ext.settings.get("cooldown_minutes") is not None else 10)


def takes_runs(seen: dict | None, now: float | None = None) -> bool:
    """Whether an account can take a run at all: its token wasn't refused and it isn't full."""
    return not (seen or {}).get("needs_auth") and (accounts.used(seen, now) or 0) < 100


def why_left(label: str | None, seen: dict | None) -> str:
    """Why runs moved off an account, in a few words."""
    if label is None:
        return "first pick"
    if (seen or {}).get("needs_auth"):
        return f"{label}'s token was refused"
    full = accounts.used(seen)
    return f"{label} was at {full:.0f}%" if full is not None else f"{label} had no room known"


def moved(was: str | None, to: str, why: str) -> None:
    """A swap: kept for the machine's status (Machines, `mb health`, rota.status) and logged."""
    now = time.time()
    if was:
        state["left"][was] = now
    state["moves"] = (state["moves"] + [{"at": now, "from": was, "to": to, "why": why}])[-MOVES_KEPT:]
    ext.log(f"runs start on {to} now (was {was}): {why}")


def summary() -> dict:
    """What the apps and agents see: labels, how full, when each resets. Never a token."""
    toks = all_tokens()
    now = time.time()
    cx = state["codex"]
    return {"current": state["current"], "threshold": threshold(), "folders": dict(state["folders"]),
            "agents": dict(state["agents"]), "moves": list(state["moves"]), "accounts": [
        {"label": label, "own_login": label == accounts.DEFAULT, "used": accounts.used(state["seen"].get(label), now),
         **{k: v for k, v in (state["seen"].get(label) or {}).items() if k in ("five_hour", "seven_day", "status",
                                                                              "needs_auth", "checked_at")},
         "line": accounts.describe(label, state["seen"].get(label), state["current"], now)}
        for label in toks],
        "codex": {"current": cx["current"], "accounts": [
            {"label": label, "own_login": label == accounts.DEFAULT, "used": accounts.used(cx["seen"].get(label), now),
             "line": accounts.describe(label, cx["seen"].get(label), cx["current"], now)}
            for label in codex.labels(accounts.load_store())]}}


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
    if p.get("client") == "codex":
        return prepare_codex()
    if p.get("client") != "claude":
        return {}
    toks = all_tokens()
    # an agent mapped to an account, else its folder, while that account can take runs (cswap's map)
    for pinned in (state["agents"].get(str(p.get("handle") or "")), state["folders"].get(folder_key(p.get("folder")))):
        if pinned in toks and takes_runs(state["seen"].get(pinned)):
            return {"env": {"CLAUDE_CODE_OAUTH_TOKEN": toks[pinned]} if toks[pinned] else {}, "account": pinned}
    with lock:
        current, now = state["current"], time.time()
        # one runs moved off lately sits out the cooldown, unless nothing else is left
        rested = [label for label in toks if now - float(state["left"].get(label) or 0) >= cooldown()]
        label = accounts.choose([lb for lb in toks if lb in rested or lb == current] or list(toks), state["seen"],
                                current, threshold())
        if label != current:
            moved(current, label, why_left(current, state["seen"].get(current)))
            state["current"] = label
            save_state()
            ext.event("rota.accounts", summary())
    return {"env": {"CLAUDE_CODE_OAUTH_TOKEN": toks[label]} if toks[label] else {}, "account": label}


def prepare_codex() -> dict:
    """A Codex run's account: its own Codex home (CODEX_HOME), or nothing for the machine's own login."""
    labels = codex.labels(accounts.load_store()) or [accounts.DEFAULT]
    cx = state["codex"]
    with lock:
        label = accounts.choose(labels, cx["seen"], cx["current"], threshold())
        if label != cx["current"]:
            ext.log(f"Codex runs start on {label} now (was {cx['current']})")
            cx["current"] = label
            save_state()
    return {"env": {} if label == accounts.DEFAULT else {"CODEX_HOME": str(codex.home(label))}, "account": label}


@ext.method("session.ended")
def ended(p: dict) -> dict:
    """A run's account as Claude Code last reported it, and whether its token was refused. A Codex run's
    from the newest session Codex wrote in that account's home."""
    label = p.get("account") or accounts.DEFAULT
    error = p.get("error")
    if p.get("client") == "codex":
        got = codex.latest(label)
        with lock:
            seen = state["codex"]["seen"].setdefault(label, {})
            if got:
                for k in ("five_hour", "seven_day"):
                    seen.pop(k, None)
                seen.update(got)
                seen["needs_auth"] = False
            elif probe.refused(error):
                seen["needs_auth"] = True
            seen["checked_at"] = time.time()
            save_state()
        ext.event("rota.accounts", summary())
        return {}
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
        why = accounts.describe(label, seen, state["current"], now)
        last = state["moves"][-1] if state["moves"] else None
        if label == state["current"] and last and last.get("to") == label and last.get("from"):
            why += f" since {time.strftime('%H:%M', time.localtime(last['at']))} ({last['why']})"
        out.append({"id": label, "kind": "account", "name": f"Claude: {label}",
                    "state": "failing" if refused or (full is not None and full >= 100) else
                    "unknown" if full is None else "ok",
                    "needs": ["auth"] if refused else [],
                    "why": why})
    cx = state["codex"]
    for label in codex.labels(accounts.load_store()):
        seen = cx["seen"].get(label) or {}
        full = accounts.used(seen, now)
        refused = bool(seen.get("needs_auth"))
        out.append({"id": f"codex.{label}", "kind": "account", "name": f"Codex: {label}",
                    "state": "failing" if refused or (full is not None and full >= 100) else
                    "unknown" if full is None else "ok",
                    "needs": ["auth"] if refused else [],
                    "why": accounts.describe(label, seen, cx["current"], now)})
    return out


@ext.method("health.fix")
def fix(p: dict) -> dict:
    label = p.get("part")
    if p.get("action") == "auth" and str(label).startswith("codex."):
        return {"message": f"On {ext.node.get('machine')}: run `python3 -m rota codex add {label[6:]}` in the "
                           "rota folder and sign in again"}
    if p.get("action") != "auth" or label == accounts.DEFAULT:
        raise ValueError("rota can only say how to sign an added account in again")
    return {"message": f"On {ext.node.get('machine')}: run `claude setup-token` signed in as {label}'s account, "
                       f"then `python3 -m rota add {label}` in the mb-rota folder and paste the token"}


@ext.command("rota.status")
def status(job) -> dict:
    return summary()


def folder_key(folder) -> str | None:
    return str(Path(folder).expanduser().resolve()) if folder else None


def label_of(p: dict, key: str = "account") -> str:
    label = str(p.get(key) or "").strip().lower()
    if not accounts.LABEL.fullmatch(label):
        raise ValueError(f"{key}: an account's label, like work")
    return label


@ext.command("rota.swap")
def swap(job) -> dict:
    """New runs start on this account from now on (or, with none named, the one with the most room), and
    stay there until it's full, as after any move."""
    toks = all_tokens()
    with lock:
        to = label_of(job.params, "to") if job.params.get("to") else accounts.most_room(list(toks), state["seen"])
        if to not in toks:
            raise ValueError(f"no account {to!r} to start runs on here: {', '.join(toks)}")
        if (state["seen"].get(to) or {}).get("needs_auth"):
            raise ValueError(f"{to}'s token was refused: sign it in again first")
        was, state["current"] = state["current"], to
        if to != was:
            moved(was, to, "asked")
        save_state()
    ext.event("rota.accounts", summary())
    return {"message": f"New runs on {ext.node.get('machine')} start on {to}" + (f" (was {was})" if was else ""),
            **summary()}


@ext.command("rota.hold")
def hold(job) -> dict:
    """Keep runs off an account until it's put back (`use`): its token stays where it is."""
    label = label_of(job.params)
    if label == accounts.DEFAULT:
        raise ValueError("the machine's own login can't be held: it's what runs fall back to")
    accounts.hold(label, job.params.get("held", True) is not False)
    ext.event("rota.accounts", summary())
    return {"message": f"runs stay off {label}" if job.params.get("held", True) is not False
            else f"{label} is back in turn", **summary()}


@ext.command("rota.agent")
def agent(job) -> dict:
    """An agent's runs start on this account while it has room (none named: back to the rule)."""
    handle = str(job.params.get("agent") or "").strip().lstrip("@").lower()
    if not handle or not accounts.LABEL.fullmatch(handle.replace("-", "_")[:32]):
        raise ValueError("agent: its handle, like scene")
    label = label_of(job.params) if job.params.get("account") else None
    with lock:
        if label:
            state["agents"][handle] = label
        else:
            state["agents"].pop(handle, None)
        save_state()
    ext.event("rota.accounts", summary())
    return {"message": f"@{handle}'s runs start on {label}" if label else f"@{handle}'s runs follow the rule again",
            **summary()}


@ext.command("rota.folder")
def folder(job) -> dict:
    """Runs in this folder start on this account while it has room (none named: back to the rule)."""
    key = folder_key(job.params.get("folder"))
    if not key:
        raise ValueError("folder: an agent's folder on this machine")
    label = label_of(job.params) if job.params.get("account") else None
    with lock:
        if label:
            state["folders"][key] = label
        else:
            state["folders"].pop(key, None)
        save_state()
    ext.event("rota.accounts", summary())
    return {"message": f"runs in {key} start on {label}" if label else f"runs in {key} follow the rule again",
            **summary()}


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
