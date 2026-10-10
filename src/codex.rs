//! Codex accounts (MB-45): each one is a Codex home of its own, signed in once with `codex login`.
//!
//! - **The accounts:** this machine's own Codex login (`default`, ~/.codex, or $CODEX_HOME) and any added
//!   with `rota codex add <label>`, each a folder under ~/.config/message-board/rota/codex/, readable by
//!   this user only. Codex keeps the account's sign-in there (auth.json) and refreshes it itself, so its
//!   tokens never leave that folder. The machine's own config.toml is linked in, so runs on any account
//!   have the same settings and servers.
//! - **A run's account:** the node asks rota before each Codex run (`session.prepare`), and the run gets
//!   `CODEX_HOME` set to that account's folder (nothing for the machine's own).
//! - **What rota knows of each** comes from Codex itself: every session it writes (sessions/…/rollout-*.jsonl
//!   in the account's home) has the account's windows in its `rate_limits` (`used_percent`,
//!   `window_minutes`, `resets_at`). Rota reads the newest after each run.
//! - **Its usage as Codex's /status shows it** (James, Pointman thread 3359 [7694]): every `check_minutes`,
//!   and for `/rota usage`, rota asks Codex's app server (`codex app-server`, JSON-RPC on its stdin) for
//!   `account/rateLimits/read` in that account's home: its windows, its plan, its credits and any free
//!   resets. That takes no turn and under a second, so an account Codex is used on outside Pointman (the
//!   machine's own, in a terminal) shows its real use too.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};

use crate::accounts::{self, Account, CodexPlan, Seen, Store, Window, DEFAULT};
use crate::util::{self, round1, truthy};

pub fn own_home() -> PathBuf {
    util::expand(&util::env("CODEX_HOME").unwrap_or_else(|| "~/.codex".into()))
}

pub fn homes_dir() -> PathBuf {
    util::expand(&util::env("ROTA_CODEX_HOMES").unwrap_or_else(|| "~/.config/message-board/rota/codex".into()))
}

pub fn home(label: &str) -> PathBuf {
    if label == DEFAULT {
        own_home()
    } else {
        homes_dir().join(label)
    }
}

/// Codex itself: on $PATH, else where Homebrew puts it.
pub fn find() -> Option<PathBuf> {
    util::which("codex").or_else(|| Some(PathBuf::from("/opt/homebrew/bin/codex")).filter(|c| c.exists()))
}

/// Whether this machine has Codex: installed, or an account added.
pub fn in_use(store: &Store) -> bool {
    util::which("codex").is_some() || !store.codex.is_empty() || own_home().join("auth.json").exists()
}

/// Whether the account is signed in: Codex keeps its sign-in in its home's auth.json.
pub fn signed_in(label: &str) -> bool {
    home(label).join("auth.json").exists()
}

/// Each usable Codex account, the machine's own first: the added ones signed in. None on a
/// machine without Codex.
pub fn labels(store: &Store) -> Vec<String> {
    if !in_use(store) {
        return vec![];
    }
    let mut out = vec![DEFAULT.to_string()];
    for a in &store.codex {
        if a.label != DEFAULT && home(&a.label).join("auth.json").exists() {
            out.push(a.label.clone());
        }
    }
    out
}

/// Codex's rate_limits as rota keeps an account's: {five_hour, seven_day: {used, resets}}, by each
/// window's length (5 h, a week). A session's are snake_case (`used_percent`, `window_minutes`,
/// `resets_at`); the app server's camelCase (`usedPercent`, `windowDurationMins`, `resetsAt`).
pub fn windows(rate_limits: Option<&Value>) -> Seen {
    let mut out = Seen::default();
    for key in ["primary", "secondary"] {
        let Some(w) = rate_limits.and_then(|r| r.get(key)).filter(|w| w.is_object()) else { continue };
        let field = |snake: &str, camel: &str| w.get(snake).or_else(|| w.get(camel)).and_then(Value::as_f64);
        let Some(used) = field("used_percent", "usedPercent") else { continue };
        let window = Window { used: round1(used), resets: field("resets_at", "resetsAt") };
        match field("window_minutes", "windowDurationMins").map(|m| m.trunc() as i64) {
            Some(300) => out.five_hour = Some(window),
            Some(10080) => out.seven_day = Some(window),
            _ => {}
        }
    }
    out
}

fn rollouts(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            rollouts(&path, out);
        } else {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                    out.push((modified, path));
                }
            }
        }
    }
}

/// The account's windows as its newest Codex session last reported them, or None.
pub fn latest(label: &str) -> Option<Seen> {
    let mut found = vec![];
    rollouts(&home(label).join("sessions"), &mut found);
    let (_, newest) = found.into_iter().max_by_key(|(modified, _)| *modified)?;
    let text = String::from_utf8_lossy(&fs::read(newest).ok()?).into_owned();
    let line = text.lines().rfind(|l| l.contains("\"rate_limits\""))?;
    let d: Value = serde_json::from_str(line).ok()?;
    let payload = [d.get("payload"), d.get("msg")].into_iter().find(|p| truthy(*p)).flatten().unwrap_or(&d);
    let got = windows(payload.get("rate_limits"));
    (got.five_hour.is_some() || got.seven_day.is_some()).then_some(got)
}

/// What the app server's `account/rateLimits/read` says of an account, as rota keeps it: its windows; its
/// status, `allowed` or why it can't take runs (`rateLimitReachedType`, or `usage_not_allowed`), blocked
/// until its full window resets; and its plan, credits and free resets.
pub fn read(got: &Value) -> Seen {
    let limits = got.get("rateLimitsByLimitId").and_then(|all| all.get("codex")).or_else(|| got.get("rateLimits"));
    let limits = limits.filter(|l| l.is_object());
    let mut out = windows(limits);
    let reached = limits.and_then(|l| l.get("rateLimitReachedType")).and_then(Value::as_str);
    let blocked = match (reached, got.get("ordinaryUsageAllowed").and_then(Value::as_bool)) {
        (Some(why), _) => Some(why.to_string()),
        (None, Some(false)) => Some("usage_not_allowed".to_string()),
        _ => None,
    };
    out.status = Some(blocked.clone().unwrap_or_else(|| "allowed".into()));
    if blocked.is_some() {
        // until the fullest window resets: the one that's out, or the later of the two
        let mut ws: Vec<&Window> = out.five_hour.iter().chain(out.seven_day.iter()).collect();
        ws.sort_by(|a, b| b.used.partial_cmp(&a.used).unwrap_or(std::cmp::Ordering::Equal));
        out.blocked_until = ws.first().and_then(|w| w.resets).filter(|r| *r != 0.0);
    }
    let credits = limits.and_then(|l| l.get("credits")).filter(|c| c.is_object());
    out.codex = Some(CodexPlan {
        plan: limits.and_then(|l| l.get("planType")).and_then(Value::as_str).map(String::from),
        has_credits: credits.and_then(|c| c.get("hasCredits")).and_then(Value::as_bool).unwrap_or(false),
        unlimited: credits.and_then(|c| c.get("unlimited")).and_then(Value::as_bool).unwrap_or(false),
        balance: credits.and_then(|c| c.get("balance")).and_then(Value::as_str).map(String::from),
        free_resets: got.get("rateLimitResetCredits").and_then(|r| r.get("availableCount")).and_then(Value::as_i64),
    });
    out
}

/// Ask Codex for an account's usage: its app server, in the account's home, `initialize` then
/// `account/rateLimits/read`, and it's stopped. `codex` is Codex itself (None: [`find`]).
pub fn check(label: &str, codex: Option<&Path>, timeout: Duration) -> anyhow::Result<Seen> {
    let codex = codex.map(Path::to_path_buf).or_else(find).ok_or_else(|| anyhow!("Codex isn't installed here"))?;
    let mut cmd = Command::new(&codex);
    cmd.arg("app-server").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if label != DEFAULT {
        cmd.env("CODEX_HOME", home(label));
    }
    let mut child = cmd.spawn().with_context(|| format!("couldn't run {}", codex.display()))?;
    let mut stdin = child.stdin.take().context("no stdin for Codex")?;
    let stdout = child.stdout.take().context("no stdout from Codex")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        }
    });
    let mut say = |msg: Value| writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
    let asked = (|| -> anyhow::Result<Value> {
        say(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"clientInfo": {"name": "rota", "version": env!("CARGO_PKG_VERSION")}}}))?;
        let until = Instant::now() + timeout;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let msg = rx.recv_timeout(left).map_err(|_| anyhow!("Codex didn't answer in {}s", timeout.as_secs()))?;
            match msg.get("id").and_then(Value::as_i64) {
                Some(1) => {
                    if let Some(e) = msg.get("error") {
                        bail!("Codex: {}", e.get("message").and_then(Value::as_str).unwrap_or("couldn't start"));
                    }
                    say(json!({"jsonrpc": "2.0", "method": "initialized"}))?;
                    say(json!({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read", "params": null}))?;
                }
                Some(2) => {
                    if let Some(e) = msg.get("error") {
                        bail!("Codex: {}", e.get("message").and_then(Value::as_str).unwrap_or("no usage"));
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                _ => {} // its notifications
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    let got = asked?;
    let seen = read(&got);
    if seen.five_hour.is_none() && seen.seven_day.is_none() && seen.codex.as_ref().is_none_or(|c| c.plan.is_none()) {
        bail!("Codex had no usage for {label}");
    }
    Ok(seen)
}

/// `rota codex add <label>`: a Codex home for the account, signed in with `codex login` (Codex opens the
/// browser), the machine's config linked in, and the label kept.
pub fn add(label: &str, store_path: Option<&Path>, codex: Option<&Path>) -> anyhow::Result<String> {
    let codex = codex.map(Path::to_path_buf).or_else(find).unwrap_or_else(|| PathBuf::from("/opt/homebrew/bin/codex"));
    let at = home(label);
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&at)?;
    fs::set_permissions(&at, fs::Permissions::from_mode(0o700))?;
    let mine = own_home().join("config.toml");
    let linked = at.join("config.toml");
    if mine.exists() && linked.symlink_metadata().is_err() {
        std::os::unix::fs::symlink(&mine, &linked)?;
    }
    let ok = Command::new(&codex)
        .arg("login")
        .env("CODEX_HOME", &at)
        .status()
        .with_context(|| format!("couldn't run {}", codex.display()))?
        .success();
    if !ok || !at.join("auth.json").exists() {
        bail!("codex login didn't finish: {label} isn't signed in ({})", at.display());
    }
    let mut store = accounts::load_store(store_path);
    store.codex.retain(|a| a.label != label);
    store.codex.push(Account::new(label));
    accounts::save_store(&store, store_path)?;
    Ok(format!("added Codex account {label} ({}). New Codex runs can start on it from now on.", at.display()))
}
