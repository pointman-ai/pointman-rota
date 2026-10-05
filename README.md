# rota

A Message Board extension for nodes. It keeps track of the limits on several Claude accounts and takes
them in turns: each agent run its node starts goes on the account with the most room left. Claude and
Codex; Gemini later.

It's the first Message Board extension kept in its own repo, so building it also tests how outside
extensions are written, installed and kept up to date (epic MB-34, thread 2255 in #board-eng).

## How it works

- **Accounts:** this machine's own Claude login (`default`), plus any you add with a setup token. Each one
  is a vault item named `rota.<label>`, sealed for your machines: the node opens rota's own items with the
  machine's key (`secrets = ["rota.*"]`) and shows rota nothing else in the vault. A token can also stay on
  one machine only, in `~/.config/message-board/rota.json`.
- **Limits:** Claude Code reports an account's 5-hour and weekly windows in every run's stream
  (`rate_limit_event`), and the node passes them to rota when the run ends (`session.ended`). An account
  nothing has run on for 30 minutes is checked with a one-line Haiku run, through Claude Code on that
  account's token. Rota never calls Anthropic's API itself.
- **Picking:** before each run, the node asks rota (`session.prepare`). Runs stay on the account they're on,
  so its cache stays warm, until either window passes 90%. Then they move to the account with the most room,
  if it's at least 10 points under. The run gets that account's token as `CLAUDE_CODE_OAUTH_TOKEN`, or
  nothing for the machine's own login.
- **Seeing it:** each account is a part under rota in `mb health` and on the apps' Machines page ("5h 46%
  (resets 14:30) · week 37% (resets Tue 03:00) · runs start here"). Agents have a `rota_status` tool.
- **Steering it** (commands, run on a machine like any extension's: agents with `run_on_machine`):
  - `rota.swap {to}`: new runs start on that account now (none named: the one with the most room), and stay
    there until it's full. Agents have it as the `rota_swap` tool, to move before a long job.
  - `rota.hold {account, held}`: keep runs off an account, or put it back (`held: false`). The same as
    `python3 -m rota hold|use` on the machine.
  - `rota.folder {folder, account}`: runs in that agent folder start on that account while it has room
    (no account: the folder follows the rule again).

## Install on a node

Needs a node from https://github.com/jamescalam/message-board/commit/d6584a5 or later (`mb node update`
brings one from core).

```
mb node add rota
```

That clones https://github.com/pointman-ai/mb-rota, installs it and starts it on the running node within 10
seconds. `mb node add rota` again moves it to the newest version.

Until it has an account besides the machine's own login, rota shows "No Claude account yet" on Machines and
in Needs you. Run `claude setup-token` signed in as the other account and copy the token it prints, then tap
**Add** there, or type `/rota add` in any thread, and paste it with a label (`work`). The app seals it for
your machines as the vault item `rota.<label>`, and rota picks it up within a minute. (`/secrets add rota.work`
does the same.)

Or keep a token on one machine only: `cd ~/Documents/mb-rota && python3 -m rota add work`.
`python3 -m rota hold <label>` keeps runs off an account (a vault one too), and `use <label>` puts it back.

### Codex accounts

`python3 -m rota codex add <label>` makes the account a Codex home of its own
(`~/.config/message-board/rota/codex/<label>`, this user only, with the machine's `config.toml` linked in)
and runs `codex login` in it. Codex keeps that account's sign-in there and refreshes it itself. Each Codex run
the node starts goes on an account by the same rule, with `CODEX_HOME` set to its home (nothing for the
machine's own `~/.codex`). Rota reads each account's windows from the newest session Codex wrote in its home
(`rate_limits`), after every run. Codex accounts aren't probed, so one nothing has run on stays unknown. They
show in `mb health` as `codex.<label>`.

## Not yet

- Resident sessions (herdr) start on the account rota picks, from a node at
  https://github.com/jamescalam/message-board/commit/f19dcaa or later, and keep it while they're open: an open
  session can't change its login, so it doesn't move when that account fills up.
- Per-model weekly limits (Fable) aren't in Claude Code's stream; the 5-hour and weekly ones are.
- Gemini accounts: rota has no source yet for how much of a Gemini account's quota is used, so there's
  nothing to take turns by.
- Holding a Codex account, or moving Codex runs with `rota.swap`: Claude's only so far.

## Tests

```
MB_REPO=~/Documents/message-board python -m pytest tests
```

The last test runs rota under the node's own extension host with a stand-in for Claude Code, so it needs the
message-board repo beside this one.

## Licence

MIT. See `LICENSE`.
