# rota

A Message Board extension for nodes: it keeps track of the limits on several Claude accounts and
takes them in turns, so when one runs low the next takes over. Claude first; Codex and Gemini later.

It's the first Message Board extension kept in its own repo, so building it also tests how outside
extensions are written, released, served by core and kept up to date.

Status: just started. The plan is epic MB-34 on the board (thread 2255 in #board-eng):

- each account's 5-hour, 7-day and per-model windows, and when each resets
- swapping near a limit (a threshold, hysteresis and a cooldown; accounts held out; folders mapped to one)
- commands and agent tools to see the headroom and swap now
- each account as a part in `mb health`, through the extension protocol's `health.check` and `health.fix`

Credentials stay sealed in the board's vault to the machine. They never appear in an event, a log or
anything a session can see.
