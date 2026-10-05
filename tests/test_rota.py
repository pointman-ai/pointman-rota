"""Rota: the rule that picks an account, the store of tokens, and the extension under the board's own node
host with a stand-in for Claude Code (needs the message-board repo: MB_REPO, or ../message-board)."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from rota import accounts  # noqa: E402

LIMITS = {"status": "allowed", "rateLimitType": "five_hour", "resetsAt": 4102444800,
          "unifiedWindows": {"five_hour": {"resetsAt": 4102444800, "utilization": 0.46},
                             "seven_day": {"resetsAt": 4102444800, "utilization": 0.37}}}
WORK = "sk-ant-oat01-" + "w" * 40
SPARE = "sk-ant-oat01-" + "s" * 40


def seen(five, week=0.0, **kw):
    return {"five_hour": {"used": five, "resets": 4102444800}, "seven_day": {"used": week, "resets": 4102444800}, **kw}


def test_windows_and_how_full():
    w = accounts.windows(LIMITS)
    assert w == {"five_hour": {"used": 46.0, "resets": 4102444800}, "seven_day": {"used": 37.0, "resets": 4102444800},
                 "status": "allowed"}
    assert accounts.used(w) == 46.0 and accounts.used({}) is None
    # a window that has reset counts as empty; over its limit counts as full until it resets
    assert accounts.used({"five_hour": {"used": 99.0, "resets": 1}, "seven_day": {"used": 20.0, "resets": None}}) == 20
    assert accounts.used({**w, "blocked_until": 4102444800}) == 100.0


def test_the_rule():
    labels = ["default", "work", "spare"]
    fill = {"default": seen(50), "work": seen(10), "spare": seen(30)}
    # runs stay where they are while it has room: a session's cache stays warm
    assert accounts.choose(labels, fill, "default", 90) == "default"
    # past the threshold they move to the one with the most room
    fill["default"] = seen(91)
    assert accounts.choose(labels, fill, "default", 90) == "work"
    # not to one that's nearly as full: stay put rather than flip back and forth
    fill = {"default": seen(92), "work": seen(85), "spare": seen(88, 95)}
    assert accounts.choose(labels, fill, "default", 90) == "default"
    # one not checked yet beats staying on a full one
    assert accounts.choose(labels + ["new"], fill, "default", 90) == "new"
    # an account whose token was refused is skipped
    fill = {"default": seen(95), "work": seen(5, needs_auth=True), "spare": seen(40)}
    assert accounts.choose(labels, fill, "default", 90) == "spare"
    assert accounts.choose([], {}, None, 90) == "default"


def test_describe():
    line = accounts.describe("work", seen(46, 37), "work", now=4102444800 - 3600)
    assert line.startswith("5h 46% (resets ") and "week 37%" in line and line.endswith("runs start here")
    assert accounts.describe("spare", None, "work") == "not checked yet"


def cli(*args, store: Path, stdin: str = "") -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, "-m", "rota", *args], cwd=ROOT, input=stdin, capture_output=True, text=True,
                          env={**os.environ, "ROTA_STORE": str(store)})


def test_the_cli_keeps_tokens_for_this_user_only(tmp_path):
    store = tmp_path / "rota.json"
    assert cli("add", "work", store=store, stdin="not a token\n").returncode == 1
    r = cli("add", "Work", store=store, stdin=WORK + "\n")
    assert r.returncode == 0, r.stderr
    assert stat.S_IMODE(store.stat().st_mode) == 0o600
    assert json.loads(store.read_text())["accounts"] == [{"label": "work", "token": WORK}]
    out = cli("list", store=store).stdout
    assert "work" in out and WORK not in out
    assert cli("hold", "work", store=store).returncode == 0
    assert accounts.tokens(accounts.load_store(store)) == {"default": None}
    cli("use", "work", store=store)
    assert accounts.tokens(accounts.load_store(store)) == {"default": None, "work": WORK}
    assert cli("add", "default", store=store, stdin=WORK + "\n").returncode == 2
    cli("remove", "work", store=store)
    assert accounts.load_store(store)["accounts"] == []
    # an account kept in the vault is held here by its label alone
    assert cli("hold", "spare", store=store).returncode == 0
    assert accounts.load_store(store)["accounts"] == [{"label": "spare", "held": True}]


# ------------------------------------------------------------ under the node's own host

MB = Path(os.environ.get("MB_REPO") or ROOT.parent / "message-board")
FAKE = """#!/bin/sh
# Claude Code, as far as rota's check needs: one rate_limit_event, how full by which token it's given
case "$CLAUDE_CODE_OAUTH_TOKEN" in
  "") used=0.95 ;;
  *bad*) echo '{"type":"result","is_error":true,"result":"OAuth token has expired"}'; exit 1 ;;
  *) used=0.2 ;;
esac
echo "$CLAUDE_CODE_OAUTH_TOKEN" >> "$FAKE_CALLS"
echo '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":'$used',"resetsAt":4102444800},"seven_day":{"utilization":0.1,"resetsAt":4102444800}}}}'
echo '{"type":"result","is_error":false,"result":"OK."}'
"""


@pytest.fixture
def host(tmp_path):
    if not (MB / "src" / "messageboard").is_dir():
        pytest.skip("needs the message-board repo beside this one (or MB_REPO)")
    sys.path.insert(0, str(MB / "src"))
    from messageboard.extensions.host import ExtensionHost

    bin_ = tmp_path / "bin"
    bin_.mkdir()
    (bin_ / "claude").write_text(FAKE)
    (bin_ / "claude").chmod(0o755)
    store = tmp_path / "rota.json"
    bad = "sk-ant-oat01-" + "bad" * 10
    accounts.save_store({"accounts": [{"label": "work", "token": WORK}, {"label": "old", "token": bad}]}, store)
    # the board's vault, as the node shows it to rota: the names rota.* covers, and their values
    held = {"rota.spare": f"CLAUDE_CODE_OAUTH_TOKEN={SPARE}\n", "rota.Not A Label": SPARE}
    h = ExtensionHost(ROOT, node="studio", machine="studio", state_dir=tmp_path / "state",
                      env={"PATH": f"{bin_}:{os.environ['PATH']}", "ROTA_STORE": str(store),
                           "FAKE_CALLS": str(tmp_path / "calls")},
                      secrets=lambda name: held[name], secret_names=lambda prefix: sorted(held))
    h.start()
    assert h.wait_ready(20), h.error
    yield h
    h.stop()


def parts(host, until, timeout=30):
    deadline = time.time() + timeout
    while True:
        got = {p["id"]: p for p in host.call("health.check", {})}
        if until(got) or time.time() > deadline:
            return got
        time.sleep(0.2)


def test_rota_on_a_node(host, tmp_path):
    # it checks each account once at start, through Claude Code, on each one's own token
    got = parts(host, lambda g: all(p["state"] != "unknown" for p in g.values()))
    assert got["default"]["why"].startswith("5h 95%")
    assert got["work"]["why"].startswith("5h 20%") and got["work"]["state"] == "ok"
    assert got["old"]["needs"] == ["auth"] and got["old"]["state"] == "failing"
    # an account kept in the vault, as rota.spare, is one too; a name that isn't a label isn't
    assert got["spare"]["why"].startswith("5h 20%") and "Not A Label" not in str(got)
    # the machine's own login is past 90%, so runs start on work, with its token
    run = host.call("session.prepare", {"run": 1, "handle": "scene", "client": "claude"})
    assert run == {"env": {"CLAUDE_CODE_OAUTH_TOKEN": WORK}, "account": "work"}
    assert host.call("session.prepare", {"run": 2, "handle": "scene", "client": "codex"}) == {}
    # a run's own report moves it on: work fills up, so runs go to the vault's spare, with its token
    host.call("session.ended", {"run": 1, "account": "work", "status": "done", "error": None, "limits": {
        "status": "allowed", "unifiedWindows": {"five_hour": {"utilization": 0.97, "resetsAt": 4102444800}}}})
    assert host.call("session.prepare", {"run": 3, "client": "claude"}) == {
        "env": {"CLAUDE_CODE_OAUTH_TOKEN": SPARE}, "account": "spare"}
    # and spare full too: nothing else has room, so they stay put
    host.call("session.ended", {"run": 3, "account": "spare", "status": "done", "error": None, "limits": {
        "status": "allowed", "unifiedWindows": {"five_hour": {"utilization": 0.95, "resetsAt": 4102444800}}}})
    assert host.call("session.prepare", {"run": 4, "client": "claude"})["account"] == "spare"
    status = host.run("rota.status", {}).result
    assert status["current"] == "spare" and WORK not in json.dumps(status)
    assert [a["label"] for a in status["accounts"]] == ["default", "work", "old", "spare"]
    assert SPARE not in json.dumps(status)
    fixed = host.call("health.fix", {"part": "old", "action": "auth"})
    assert "claude setup-token" in fixed["message"] and "python3 -m rota add old" in fixed["message"]


def test_swapping_holding_and_folders(host, tmp_path):
    parts(host, lambda g: all(p["state"] != "unknown" for p in g.values()))
    assert host.call("session.prepare", {"run": 1, "client": "claude"})["account"] == "work"
    # asked to, runs move at once, and stay there as after any move
    got = host.run("rota.swap", {"to": "spare"}).result
    assert got["current"] == "spare" and got["message"] == "New runs on studio start on spare (was work)"
    assert host.call("session.prepare", {"run": 2, "client": "claude"})["account"] == "spare"
    with pytest.raises(Exception, match="token was refused"):
        host.run("rota.swap", {"to": "old"})
    with pytest.raises(Exception, match="no account 'gone'"):
        host.run("rota.swap", {"to": "gone"})
    # none named: the one with the most room (work and spare are both at 20%, default at 95%)
    assert host.run("rota.swap", {}).result["current"] in ("work", "spare")

    # held, an account is left out until it's put back; the machine's own login can't be
    assert host.run("rota.hold", {"account": "spare"}).result["message"] == "runs stay off spare"
    assert "spare" not in [a["label"] for a in host.run("rota.status", {}).result["accounts"]]
    assert host.call("session.prepare", {"run": 3, "client": "claude"})["account"] == "work"
    host.run("rota.hold", {"account": "spare", "held": False})
    assert "spare" in [a["label"] for a in host.run("rota.status", {}).result["accounts"]]
    with pytest.raises(Exception, match="can't be held"):
        host.run("rota.hold", {"account": "default"})

    # a folder's runs start on its own account, whatever the rule picks for the rest
    agent = tmp_path / "agent"
    agent.mkdir()
    host.run("rota.folder", {"folder": str(agent), "account": "spare"})
    host.run("rota.swap", {"to": "work"})
    assert host.call("session.prepare", {"run": 4, "client": "claude", "folder": str(agent)}) == {
        "env": {"CLAUDE_CODE_OAUTH_TOKEN": SPARE}, "account": "spare"}
    assert host.call("session.prepare", {"run": 5, "client": "claude", "folder": "/elsewhere"})["account"] == "work"
    # ...until it's full: then the rule again
    host.call("session.ended", {"run": 4, "account": "spare", "status": "done", "error": None, "limits": {
        "status": "rejected", "resetsAt": 4102444800,
        "unifiedWindows": {"five_hour": {"utilization": 1.0, "resetsAt": 4102444800}}}})
    assert host.call("session.prepare", {"run": 6, "client": "claude", "folder": str(agent)})["account"] == "work"
    assert host.run("rota.folder", {"folder": str(agent)}).result["folders"] == {}
