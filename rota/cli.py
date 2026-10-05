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
python3 -m rota list            the accounts in this machine's file (never their tokens)"""


def main(argv: list[str]) -> int:
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
    if named is None and cmd in ("hold", "use"):      # an account kept in the vault: held here by its label
        named = {"label": label}
        store["accounts"].append(named)
    if named is None:
        print(f"no account {label!r} in this machine's file", file=sys.stderr)
        return 1
    if cmd == "remove":
        store["accounts"] = [a for a in store["accounts"] if a["label"] != label]
    else:
        named["held"] = cmd == "hold"
    accounts.save_store(store)
    print({"remove": f"removed {label}", "hold": f"runs stay off {label}", "use": f"{label} is back in turn"}[cmd])
    return 0
