# rota

A Pointman extension that takes several Claude and Codex accounts in turns. It keeps track of each
account's limits, and each agent run a node starts goes on the account with the most room left, so agents
keep working when one account fills up.

One Rust binary, `bin/rota`, built for each platform and written with the
[pointman-extension](https://github.com/pointman-ai/pointman-extension) crate: `rota serve` is the
extension its node runs, and its other subcommands are a command line for this machine's accounts.

```
pointman add rota
```

## How it works

- **Accounts:** this machine's own Claude login (`default`), plus any you add. Each one is a vault item
  named `rota.<label>`, sealed for your machines: the node opens rota's own items with the machine's key
  (`secrets = ["rota.*"]`) and shows rota nothing else in the vault. A token can also stay on one machine
  only, in `~/.config/message-board/rota.json`.
- **Limits:** Claude Code reports an account's 5-hour and weekly windows in every run's stream, and the
  node passes them to rota when the run ends. An account nothing has run on for 30 minutes is checked with
  a one-line Haiku run, through Claude Code on that account's token. Rota never calls Anthropic's API
  itself.
- **Picking:** before each run, the node asks rota which account it goes on. Runs stay on the account
  they're on, so its cache stays warm, until either window passes 90%. Then they move to the account with
  the most room, if it's at least 10 points under. The run gets that account's token as
  `CLAUDE_CODE_OAUTH_TOKEN`, or nothing for the machine's own login.
- **A warning isn't a limit:** an account past a warning mark (the week at 52%, say) still takes runs; only
  a status that isn't allowed blocks it until its reset.
- **Cooldown:** an account runs moved off sits out 10 minutes (`cooldown_minutes`) even if it looks fine
  again, unless nothing else can take runs. Its numbers can lag, and two nearly full accounts would
  otherwise take turns run by run.
- **claude.ai connectors:** a run that's been given one (Drive, say) starts on the machine's own login
  while it can take runs, since a setup token loads none of them. An agent's own account still wins; its
  folder's comes after.
- **Usage credits:** each account's line says what happens past 100%: `monthly spend 40% ($80 of $200)`,
  `monthly spend limit reached ($200 of $200)`, `usage credits on, $12 this month, no limit`, or
  `usage credits off`. The amounts are what Claude Code's `/usage` shows, asked without taking a turn.

Agents need nothing: rota moves their runs itself, and they can post `/rota usage` like anyone.

## Seeing it

- **`/rota usage`** in any thread answers with a chart of each account's 5-hour and weekly use, which
  account runs start on, a line for each account, and buttons that move new runs.
- **The Rota pane** shows each account with a meter for each window and the same buttons, and moves in
  place as checks and swaps come in. Open it from the apps' menu.
- **The Rota panel** is the same chart on the telemetry board.
- **`pointman health`** and the apps' Machines page list each account under rota: "5h 46% (resets 14:30) ·
  week 37% (resets Tue 03:00) · runs start here", and since when and why runs are on it ("since 14:05
  (work was at 97%)"). `rota.status` keeps the last 10 moves.

## Adding a Claude account

Until it has an account besides the machine's own login, rota shows "No Claude account yet" on Machines
and in Needs you.

- **Sign in:** type `/rota add` in any thread, or use **Add a Claude account** at the bottom of the Rota
  pane, and tap **Sign in**. Rota runs `claude setup-token` on the machine and the app opens its sign-in
  link, on any device. Sign in as the other account and paste the code the page shows back into the app.
  The token goes straight into the vault, sealed for your machines, without anyone seeing it.
- **Or paste a token:** run `claude setup-token` signed in as the other account, and paste the token it
  prints into the same form with a label (`work`). `/rota add` opens a form, so nothing is posted.
  `pointman secrets add rota.work` does the same from a terminal.
- **Or keep it on one machine:** `rota add work` with rota's own binary there, then paste the token. It's
  `bin/rota` in the folder the node installed rota in (`pointman health` gives the whole path when an
  account needs signing in again). `rota remove <label>` takes one out of this machine's file, and
  `rota list` shows the file's accounts (never their tokens).

Rota picks a new account up within a minute. One in the vault goes with `pointman secrets remove
rota.<label>`.

## Codex accounts

`rota codex add <label>` makes a Codex home for the account (`~/.config/message-board/rota/codex/<label>`,
this user only, with the machine's `config.toml` linked in) and runs `codex login` in it. Codex keeps that
account's sign-in there and refreshes it itself. Each Codex run the node starts goes on an account by the
same rule, with `CODEX_HOME` set to its home (nothing for the machine's own `~/.codex`).

`/rota usage`, the pane and `pointman health` show each Codex account under its own heading, as Codex's
`/status` shows it: its 5-hour and weekly windows, its plan, its credits and any free resets ("week 1%
(resets Thu 15:41) · Business plan · 3 free resets"). Rota asks Codex's app server for these numbers,
which takes no turn and under a second, so the machine's own Codex counts what you use in a terminal too.
They show in `pointman health` as `codex.<label>`.

## Commands

Run on a machine like any extension's: `/rota swap work` in a thread, or `create_job` for agents.

| Kind | Slash | What |
|---|---|---|
| `rota.usage` | `/rota usage` | Each account's 5-hour and weekly use, and which one new runs start on |
| `rota.add` | `/rota add` | Add a Claude account: sign in, or paste a token |
| `rota.swap {to}` | `/rota swap [to]` | New runs start on that account now (none named: the one with the most room), and stay until it's full |
| `rota.rename {from, to}` | `/rota rename default personal` | The name an account is shown and typed by. Its vault item doesn't change; renamed back to its label, the name goes |
| `rota.folder {folder, account}` | | Runs in that agent folder start on that account while it has room (no account: the folder follows the rule again) |
| `rota.agent {agent, account}` | | The same for an agent, by its handle. An agent's account wins over its folder's |
| `rota.status` | | Each account, its limits, and the last 10 moves |

## Settings

| Setting | Default | |
|---|---|---|
| `threshold` | 90 | Move new runs off an account once either window is this full (%), 50–100 |
| `cooldown_minutes` | 10 | After moving runs, stay on the new account this long, unless it can't take runs at all |
| `check_minutes` | 30 | How often an account nothing has run on is checked, and each Codex account asked for its usage |

## Install, upgrade, versions

`pointman add rota` installs it on this machine's node, which starts it within 10 seconds. Each node
fetches its own platform's build, so the machine needs no clone, git or Rust. `--nodes studio,mini` (or
`--nodes all`) adds it on other nodes instead.

`pointman upgrade rota` moves it to the newest release, keeping the running version until the new one has
started, and `pointman list` shows what's installed where. Versions work as uv's: `pointman add
rota==0.13.0` (or `'rota>=0.13,<0.14'`) keeps it to them, `upgrade` stays within them, and `pointman add
rota@latest` lets them go. For development, `pointman add <folder>` installs a checkout as it is.

## Not yet

- Sessions that stay open start on the account rota picks and keep it: an open session can't change its
  login, so it doesn't move when that account fills up.
- Per-model weekly limits aren't in Claude Code's stream; the 5-hour and weekly ones are.
- Gemini accounts: there's no source yet for how much of a Gemini account's quota is used.
- Renaming a Codex account, or moving Codex runs with `rota.swap`: Claude's only so far.

## Building and testing

```
cargo test
```

`tests/accounts.rs` has the rule, the store and the command line; `tests/node.rs` runs the built binary
under the SDK's stand-in node, with a stand-in for Claude Code on its PATH (its checks, runs and sign-in).

To release, bump `version` in extension.toml and Cargo.toml (a test checks they match) and push a tag for
it (`git tag v0.13.1 && git push origin v0.13.1`). The release workflow builds rota for each platform in
`platforms` (macOS arm64 and x86_64, Linux x86_64 and arm64; Linux's are static), packs each as
`rota-<version>-<platform>.tar.gz` with extension.toml, bin/rota, README.md and LICENSE at its top, and
makes the release with all four attached. The marketplace lists it within 30 minutes.

## Licence

MIT. See `LICENSE`.
