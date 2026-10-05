# rota

A Message Board extension for nodes. It keeps track of the limits on several Claude accounts and takes
them in turns: each agent run its node starts goes on the account with the most room left. Claude first;
Codex and Gemini later.

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

## Install on a node

Needs a node from https://github.com/jamescalam/message-board/commit/d6584a5 or later (`mb node update`
brings one from core).

```
git clone git@github.com:pointman-ai/mb-rota.git ~/Documents/mb-rota && mb ext add ~/Documents/mb-rota
```

The running node starts it within 10 seconds; `mb ext add` again after a `git pull` moves it to the new
version.

Add each other account once: run `claude setup-token` signed in as that account and copy the token it prints.
Then, in any thread in the apps, type `/secrets add rota.work` and paste it. The app seals it for your
machines, and rota picks it up within a minute.

Or keep a token on one machine only: `cd ~/Documents/mb-rota && python3 -m rota add work`.
`python3 -m rota hold <label>` keeps runs off an account (a vault one too), and `use <label>` puts it back.

## Not yet

- Resident sessions (herdr) start on the machine's own login.
- Per-model weekly limits (Fable) aren't in Claude Code's stream; the 5-hour and weekly ones are.
- Codex and Gemini accounts (MB-45).

## Tests

```
MB_REPO=~/Documents/message-board python -m pytest tests
```

The last test runs rota under the node's own extension host with a stand-in for Claude Code, so it needs the
message-board repo beside this one.

## Licence

MIT. See `LICENSE`.
