# rota

A Message Board extension for nodes. It keeps track of the limits on several Claude accounts and takes
them in turns: each agent run its node starts goes on the account with the most room left. Claude and
Codex; Gemini later.

It's the first Message Board extension kept in its own repo, so building it also tests how outside
extensions are written, installed and kept up to date (epic MB-34, thread 2255 in #board-eng). Since 0.7.0
it's one Rust binary, `bin/rota`, built for each platform and written with the
[pointman-extension](https://github.com/pointman-ai/pointman-extension) crate: `rota serve` is the
extension its node runs, and the rest of its subcommands are the command line for this machine's accounts.

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
- **Seeing it:** `/rota usage` in any thread answers with a chart of each account's 5-hour and weekly use, the
  account runs start on in its title, a line for each, and buttons that move new runs (Pointman's slash
  commands and blocks, MB-276). The same chart is a panel people can add to the telemetry board
  (`rota/accounts@<machine>`). Each account is also a part under rota in `pointman health` and on the apps'
  Machines page ("5h 46% (resets 14:30) · week 37% (resets Tue 03:00) · runs start here"). Agents need
  nothing: rota moves their runs itself (James, Pointman thread 2255 [4082]), and they can post
  `/rota usage` like anyone.
- **Steering it** (commands, run on a machine like any extension's: `/rota swap [to]` and
  `/rota rename <name> <new name>` in a thread, agents with `run_on_machine`):
  - `rota.swap {to}`: new runs start on that account now (none named: the one with the most room), and stay
    there until it's full.
  - `rota.rename {from, to}`: the name an account is shown and typed by, the machine's own login too
    (`/rota rename default james-max`). Its label stays, so its vault item doesn't change. Renamed back to
    its label, the name goes.
  - `rota.folder {folder, account}`: runs in that agent folder start on that account while it has room
    (no account: the folder follows the rule again).
  - `rota.agent {agent, account}`: the same for an agent, by its handle. An agent's account wins over its
    folder's.
- **claude.ai connectors:** a run granted one (Drive, say) starts on the machine's own login while it can
  take runs, since a setup token loads none of them. The node passes a run's connectors to
  `session.prepare` as `connectors`. An agent's own account still wins; its folder's comes after.
- **Usage credits** (James, Pointman thread 649 [4622], [4634]): each account's line says what happens past
  100%: `monthly spend 40% ($80 of $200)`, `monthly spend limit reached ($200 of $200)` (Claude's "You've
  hit your monthly spend limit": runs stop when a window fills), `usage credits on, $12 this month, no
  limit`, or `usage credits off`. The amounts are what Claude Code's `/usage` shows: each check asks it
  (`get_usage`, a control request on its stream-json input), and an account runs keep busy is asked alone
  every `check_minutes`, which takes no turn. It's `credits` in rota's summary. Where Claude Code has no
  usage for an account, its runs' limit events still say whether credits are on, in use, off, or at their
  limit (`extra`).
- **A warning isn't a limit:** `allowed_warning` (a window past a mark, like the week at 52%) still takes
  runs; only a status that isn't `allowed…` blocks an account until its reset.
- **Cooldown:** an account runs moved off sits out 10 minutes (`cooldown_minutes`) even if it looks fine
  again, unless nothing else can take runs: its numbers can lag, and two nearly full accounts would otherwise
  take turns run by run.
- **Each move is on the machine's status:** the account runs start on says since when and why in `pointman health`
  and on Machines ("since 14:05 (work was at 97%)"), and `rota.status` keeps the last 10 moves.

## Install on a node

```
pointman add rota
```

That installs rota from your workspace's catalogue and starts it on the running node within 10 seconds.
Core fetched it from this repo's releases through the GitHub App, and the node fetches its own platform's
build, so the machine needs no clone, git, Python or Rust (a node that runs compiled extensions).
The first time, `pointman add rota` run by an owner brings it into the catalogue too: core finds this
repo (`pointman-rota`) on the accounts the workspace installed the GitHub App on (pointman-ai).
`pointman add rota --node studio` adds it on another machine's node instead.

`pointman upgrade rota` moves it to the newest release once core has it, keeping the running version
until the new one starts, and `pointman node list` shows what's installed and what's latest. Versions
work as uv's: `pointman add rota==0.5.1` (or `'rota>=0.5,<0.6'`) keeps it to them, `upgrade` stays
within them, and `pointman add rota@latest` lets them go. For development, `pointman add <folder>`
installs a checkout as it is.

### Releasing

Bump `version` in extension.toml and Cargo.toml (a test checks they match), then push a tag for it
(`git tag v0.7.0 && git push origin v0.7.0`). The release workflow builds rota on each platform in
`platforms` (macos-arm64, macos-x86_64, linux-x86_64 and linux-arm64; Linux's are static), packs each as
`rota-<version>-<platform>.tar.gz` with extension.toml, bin/rota, README.md and LICENSE at its top, and
makes the release with all four attached. Core takes the release event, keeps each platform's build, and
machines show rota behind until they run `pointman upgrade`.

Until it has an account besides the machine's own login, rota shows "No Claude account yet" on Machines and
in Needs you. Run `claude setup-token` signed in as the other account and copy the token it prints, then tap
**Add** there, or type `/rota add` in any thread (rota's own command: the app opens its account form for it,
so nothing is posted), and paste it with a label (`work`). The app seals it for
your machines as the vault item `rota.<label>`, and rota picks it up within a minute. (`/secrets add rota.work`
does the same.)

Or tap **Sign in** on that sheet instead (MB-216, once core and the apps have their halves): rota runs
`claude setup-token` on the machine and the app opens its sign-in link, on any device. Sign in as the other
account, paste the code the page shows back into the app, and the token goes straight into the vault, sealed
for your machines, without anyone seeing it.

Or keep a token on one machine only, with rota's own binary there: `rota add work`, then paste the token.
It's `bin/rota` in the folder the node installed rota in (`pointman health` gives the whole path when an account
needs signing in again), or `cargo run --` in a checkout. `remove <label>` takes one out of this machine's
file, and `list` shows the file's accounts (never their tokens). An account in the vault goes with
`pointman secrets remove rota.<label>`. (Holding an account went in 0.9.0: James, Pointman thread 2255 [4136].)

### Codex accounts

`rota codex add <label>` makes the account a Codex home of its own
(`~/.config/message-board/rota/codex/<label>`, this user only, with the machine's `config.toml` linked in)
and runs `codex login` in it. Codex keeps that account's sign-in there and refreshes it itself. Each Codex run
the node starts goes on an account by the same rule, with `CODEX_HOME` set to its home (nothing for the
machine's own `~/.codex`). Rota reads each account's windows from the newest session Codex wrote in its home
(`rate_limits`), after every run. Codex accounts aren't probed, so one nothing has run on stays unknown. They
show in `pointman health` as `codex.<label>`.

## Not yet

- Resident sessions (herdr) start on the account rota picks, from a node at
  https://github.com/jamescalam/message-board/commit/f19dcaa or later, and keep it while they're open: an open
  session can't change its login, so it doesn't move when that account fills up.
- Per-model weekly limits (Fable) aren't in Claude Code's stream; the 5-hour and weekly ones are.
- Gemini accounts: rota has no source yet for how much of a Gemini account's quota is used, so there's
  nothing to take turns by.
- Renaming a Codex account, or moving Codex runs with `rota.swap`: Claude's only so far.

## Tests

```
cargo test
```

`tests/accounts.rs` has the rule, the store and the command line; `tests/node.rs` runs the built binary
under the SDK's stand-in node, with a stand-in for Claude Code on its PATH (its checks, runs and sign-in),
and checks a Python rota's state file carries over. They're the Python rota's tests (0.6.1), scenario for
scenario. While pointman-extension is private, cargo fetches it with git, so it needs a clone's access to
it (and CI the `POINTMAN_EXTENSION_TOKEN` secret).

## Licence

MIT. See `LICENSE`.
