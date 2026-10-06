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
//!   `window_minutes`, `resets_at`). Rota reads the newest after each run. Codex isn't probed: an account
//!   nothing has run on stays unknown, which the rule prefers to a full one.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{bail, Context};
use serde_json::Value;

use crate::accounts::{self, Account, Seen, Store, Window, DEFAULT};
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

/// Whether this machine has Codex: installed, or an account added.
pub fn in_use(store: &Store) -> bool {
    util::which("codex").is_some() || !store.codex.is_empty() || own_home().join("auth.json").exists()
}

/// Each usable Codex account, the machine's own first: the added ones signed in and not held. None on a
/// machine without Codex.
pub fn labels(store: &Store) -> Vec<String> {
    if !in_use(store) {
        return vec![];
    }
    let mut out = vec![DEFAULT.to_string()];
    for a in &store.codex {
        if a.label != DEFAULT && !a.held() && home(&a.label).join("auth.json").exists() {
            out.push(a.label.clone());
        }
    }
    out
}

/// Codex's rate_limits as rota keeps an account's: {five_hour, seven_day: {used, resets}}, by each
/// window's length (5 h, a week).
pub fn windows(rate_limits: Option<&Value>) -> Seen {
    let mut out = Seen::default();
    for key in ["primary", "secondary"] {
        let Some(w) = rate_limits.and_then(|r| r.get(key)).filter(|w| w.is_object()) else { continue };
        let Some(used) = w.get("used_percent").filter(|u| u.is_number()).and_then(Value::as_f64) else { continue };
        let window = Window { used: round1(used), resets: w.get("resets_at").and_then(Value::as_f64) };
        match w.get("window_minutes").and_then(Value::as_f64).map(|m| m.trunc() as i64) {
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

/// `rota codex add <label>`: a Codex home for the account, signed in with `codex login` (Codex opens the
/// browser), the machine's config linked in, and the label kept.
pub fn add(label: &str, store_path: Option<&Path>, codex: Option<&Path>) -> anyhow::Result<String> {
    let codex = codex
        .map(Path::to_path_buf)
        .or_else(|| util::which("codex"))
        .unwrap_or_else(|| PathBuf::from("/opt/homebrew/bin/codex"));
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
