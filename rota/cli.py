"""`python3 -m rota add|remove|hold|use|list`: this machine's accounts, kept where only its user can read
them. A token is pasted here, never sent through the board."""

from __future__ import annotations

import getpass
import sys

from rota import accounts

USAGE = """Accounts are best kept in the board's vault as rota.<label> (from the apps, or `mb vault store`).
On this machine only:
python3 -m rota add <label>     add an account: paste the token `claude setup-token` printed
python3 -m rota remove <label>  take one out
python3 -m rota hold <label>    keep runs off it for now, a vault one too (use: put it back)
python3 -m rota list            the accounts in this machine's file (never their tokens)
python3 -m rota codex add <label>  a Codex account: signed in with `codex login` in a Codex home of its own"""


def main(argv: list[str]) -> int:
    if argv[:1] == ["codex"]:
        return codex_main(argv[1:])
    cmd, label = argv[0], (argv[1].lower() if len(argv) > 1 else "")
    store = accounts.load_store()
    named = next((a for a in store["accounts"] if a["label"] == label), None)
    if cmd == "list":
        print(f"{accounts.DEFAULT}  (this machine's own Claude login)")
        for a in store["accounts"]:
            print(f"{a['label']}{'  (held)' if a.get('held') else ''}")
        return 0
    if cmd not in ("add", "remove", "hold", "use") or not label:
        print(USAGE, file=sys.stderr)
        return 2
    if label == accounts.DEFAULT or not accounts.LABEL.fullmatch(label):
        print(f"pick another label: lowercase letters, digits, . _ -, and not {accounts.DEFAULT!r}", file=sys.stderr)
        return 2
    if cmd == "add":
        token = (getpass.getpass(f"{label}'s token (from `claude setup-token`): ") if sys.stdin.isatty()
                 else sys.stdin.readline()).strip()
        if not accounts.TOKEN.fullmatch(token):
            print("that isn't a setup token (sk-ant-oat01-…): run `claude setup-token` signed in as that account",
                  file=sys.stderr)
            return 1
        store["accounts"] = [a for a in store["accounts"] if a["label"] != label] + [{"label": label, "token": token}]
        accounts.save_store(store)
        print(f"added {label}. Rota checks it within a minute; `mb health` shows how full it is.")
        return 0
    if cmd in ("hold", "use"):                        # one kept in the vault is held here by its label
        accounts.hold(label, cmd == "hold")
    elif named is None:
        print(f"no account {label!r} in this machine's file", file=sys.stderr)
        return 1
    else:
        store["accounts"] = [a for a in store["accounts"] if a["label"] != label]
        accounts.save_store(store)
    print({"remove": f"removed {label}", "hold": f"runs stay off {label}", "use": f"{label} is back in turn"}[cmd])
    return 0


def codex_main(argv: list[str]) -> int:
    from rota import codex
    label = argv[1].lower() if len(argv) > 1 else ""
    if argv[:1] != ["add"] or not accounts.LABEL.fullmatch(label) or label == accounts.DEFAULT:
        print("python3 -m rota codex add <label>   (lowercase letters, digits, . _ -; not default)", file=sys.stderr)
        return 2
    try:
        print(codex.add(label))
    except RuntimeError as e:
        print(str(e), file=sys.stderr)
        return 1
    return 0
