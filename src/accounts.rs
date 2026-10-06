//! Rota's accounts and the rule that picks one.
//!
//! - **The accounts** are this machine's own Claude login ("default", no token: a run uses whatever Claude
//!   Code is signed in as) and any added with `rota add <label>`, each a setup token from
//!   `claude setup-token`. They're kept in ~/.config/message-board/rota.json, readable by this user only.
//! - **What rota knows of each** comes from Claude Code itself: every run's stream reports its account's
//!   5-hour and weekly windows (`rate_limit_event`), and an account nothing has run on lately is checked
//!   with a one-line Haiku run. That's kept in the extension's state folder, with no tokens in it.
//! - **The rule:** new runs stay on the account they're on until either window passes the threshold (90%),
//!   then move to the account with the most room left, if it's at least 10 points under the threshold.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::util::{self, epoch, round1};

pub const DEFAULT: &str = "default";
/// An account to move to must be this far under the threshold: no flip-flopping.
pub const MARGIN: f64 = 10.0;
static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]{0,31}$").unwrap());
pub static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"sk-ant-oat\d+-[A-Za-z0-9_-]{20,}").unwrap());

/// Whether this is an account's label: lowercase letters, digits, `.`, `_` and `-`, up to 32.
pub fn is_label(s: &str) -> bool {
    LABEL.is_match(s)
}

/// Whether this is a setup token and nothing else.
pub fn is_token(s: &str) -> bool {
    TOKEN.find(s).is_some_and(|m| m.start() == 0 && m.end() == s.len())
}

/// The first setup token in some text (a vault item can be `CLAUDE_CODE_OAUTH_TOKEN=…`).
pub fn find_token(s: &str) -> Option<String> {
    TOKEN.find(s).map(|m| m.as_str().to_string())
}

/// One account in this machine's file: its label, its token if it's kept here, and whether it's held.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Account {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<bool>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl Account {
    pub fn new(label: &str) -> Self {
        Account { label: label.to_string(), ..Default::default() }
    }

    pub fn held(&self) -> bool {
        self.held == Some(true)
    }
}

/// This machine's file: its Claude accounts and its Codex ones.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Store {
    pub accounts: Vec<Account>,
    pub codex: Vec<Account>,
}

pub fn store_path() -> PathBuf {
    util::expand(&util::env("ROTA_STORE").unwrap_or_else(|| "~/.config/message-board/rota.json".into()))
}

pub fn load_store(path: Option<&Path>) -> Store {
    let d: Value = fs::read_to_string(path.map_or_else(store_path, Path::to_path_buf))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let pick = |key: &str| -> Vec<Account> {
        d.get(key)
            .and_then(Value::as_array)
            .map(|all| {
                all.iter()
                    .filter_map(|a| serde_json::from_value::<Account>(a.clone()).ok())
                    .filter(|a| is_label(&a.label))
                    .collect()
            })
            .unwrap_or_default()
    };
    Store { accounts: pick("accounts"), codex: pick("codex") }
}

pub fn save_store(store: &Store, path: Option<&Path>) -> std::io::Result<()> {
    let path = path.map_or_else(store_path, Path::to_path_buf);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!("{}.tmp", path.file_name().unwrap_or_default().to_string_lossy()));
    // tokens: this user only
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(store)?.as_bytes())?;
    drop(f);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, &path)
}

/// Keep runs off an account, or put it back in turn. One kept in the vault is held here by its label.
pub fn hold(label: &str, held: bool, path: Option<&Path>) -> std::io::Result<()> {
    let mut store = load_store(path);
    match store.accounts.iter_mut().find(|a| a.label == label) {
        Some(named) => named.held = Some(held),
        None => store.accounts.push(Account { held: Some(held), ..Account::new(label) }),
    }
    save_store(&store, path)
}

/// Each usable account's token, in order: this machine's own login first (no token).
pub fn tokens(store: &Store) -> Vec<(String, Option<String>)> {
    let mut out = vec![(DEFAULT.to_string(), None)];
    for a in &store.accounts {
        if let (false, true, Some(token)) = (a.held(), a.label != DEFAULT, &a.token) {
            put(&mut out, &a.label, Some(token.clone()));
        }
    }
    out
}

/// An ordered map's insert, as a Python dict's: a label already there keeps its place.
pub fn put(list: &mut Vec<(String, Option<String>)>, label: &str, token: Option<String>) {
    match list.iter_mut().find(|(l, _)| l == label) {
        Some(slot) => slot.1 = token,
        None => list.push((label.to_string(), token)),
    }
}

/// One of an account's windows: how full (%) and when it resets (epoch).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Window {
    pub used: f64,
    #[serde(default, serialize_with = "epoch")]
    pub resets: Option<f64>,
}

/// What rota knows of an account: {five_hour, seven_day: {used, resets}, status, …}.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Seen {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "epoch")]
    pub blocked_until: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_auth: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Seen {
    pub fn refused(&self) -> bool {
        self.needs_auth == Some(true)
    }

    fn windows(&self) -> impl Iterator<Item = &Window> {
        self.five_hour.iter().chain(self.seven_day.iter())
    }
}

/// A `rate_limit_info` as rota keeps it: {five_hour, seven_day: {used (%), resets (epoch)}, status}.
pub fn windows(limits: Option<&Value>) -> Seen {
    let mut out = Seen::default();
    let Some(limits) = limits else { return out };
    if let Some(all) = limits.get("unifiedWindows").and_then(Value::as_object) {
        for (name, w) in all {
            let Some(used) = w.get("utilization").filter(|u| u.is_number()).and_then(Value::as_f64) else {
                continue;
            };
            let window = Window { used: round1(used * 100.0), resets: w.get("resetsAt").and_then(Value::as_f64) };
            match name.as_str() {
                "five_hour" => out.five_hour = Some(window),
                "seven_day" => out.seven_day = Some(window),
                _ => {}
            }
        }
    }
    if let Some(status) = limits.get("status").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        out.status = Some(status.to_string());
        if status != "allowed" {
            out.blocked_until = limits.get("resetsAt").and_then(Value::as_f64).filter(|t| *t != 0.0);
        }
    }
    out
}

/// How full the account is: its fuller window, a window that has reset counting as empty, and 100 while
/// Claude says it's over its limit. None: nothing known yet.
pub fn used(seen: Option<&Seen>, now: Option<f64>) -> Option<f64> {
    let seen = seen?;
    if seen.five_hour.is_none() && seen.seven_day.is_none() {
        return None;
    }
    let now = now.unwrap_or_else(util::now);
    if seen.blocked_until.is_some_and(|t| t != 0.0 && now < t) {
        return Some(100.0);
    }
    seen.windows().map(|w| if w.resets.is_some_and(|r| r != 0.0 && now >= r) { 0.0 } else { w.used }).reduce(f64::max)
}

fn fuller(a: &f64, b: &f64) -> std::cmp::Ordering {
    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
}

/// The account a new run starts on. `labels` are the usable ones, in order (the machine's own first).
pub fn choose(
    labels: &[String],
    seen: &BTreeMap<String, Seen>,
    current: Option<&str>,
    threshold: f64,
    now: Option<f64>,
) -> String {
    if labels.is_empty() {
        return DEFAULT.to_string();
    }
    let fill: BTreeMap<&str, Option<f64>> = labels.iter().map(|l| (l.as_str(), used(seen.get(l), now))).collect();
    let mut usable: Vec<&String> = labels.iter().filter(|l| !seen.get(*l).is_some_and(Seen::refused)).collect();
    if usable.is_empty() {
        usable = labels.iter().collect();
    }
    if let Some(current) = current.and_then(|c| usable.iter().find(|l| l.as_str() == c)) {
        if fill[current.as_str()].is_none_or(|f| f < threshold) {
            return current.to_string();
        }
    }
    let best = usable.iter().filter_map(|l| fill[l.as_str()].map(|f| (*l, f))).min_by(|a, b| fuller(&a.1, &b.1));
    if let Some((label, f)) = best {
        if f <= threshold - MARGIN {
            return label.clone();
        }
    }
    // nothing known yet beats one known to be full
    if let Some(unknown) = usable.iter().find(|l| fill[l.as_str()].is_none()) {
        return unknown.to_string();
    }
    if let Some(current) = current.filter(|c| usable.iter().any(|l| l == c)) {
        return current.to_string();
    }
    best.map_or_else(|| usable[0].clone(), |(label, _)| label.clone())
}

/// The account with the most room left, of those not refused: one never checked counts as empty.
pub fn most_room(labels: &[String], seen: &BTreeMap<String, Seen>, now: Option<f64>) -> Option<String> {
    labels
        .iter()
        .filter(|l| !seen.get(*l).is_some_and(Seen::refused))
        .map(|l| (l, used(seen.get(l), now).unwrap_or(0.0)))
        .min_by(|a, b| fuller(&a.1, &b.1))
        .map(|(l, _)| l.clone())
}

/// "5h 46% (resets 14:30) · week 37% (resets Tue 03:00) · runs start here", for health and status.
pub fn describe(label: &str, seen: Option<&Seen>, current: Option<&str>, now: Option<f64>) -> String {
    let now = now.unwrap_or_else(util::now);
    let mut bits = vec![];
    for (w, name) in
        [(seen.and_then(|s| s.five_hour.as_ref()), "5h"), (seen.and_then(|s| s.seven_day.as_ref()), "week")]
    {
        let Some(w) = w else { continue };
        let resets = w.resets.filter(|r| *r != 0.0);
        if resets.is_some_and(|r| now >= r) {
            bits.push(format!("{name} reset"));
            continue;
        }
        let when = resets.map(|r| util::local_time(r, if r - now < 20.0 * 3600.0 { "%H:%M" } else { "%a %H:%M" }));
        bits.push(match when {
            Some(when) => format!("{name} {:.0}% (resets {when})", w.used),
            None => format!("{name} {:.0}%", w.used),
        });
    }
    if bits.is_empty() {
        bits.push("not checked yet".into());
    }
    if seen.is_some_and(Seen::refused) {
        bits.push("its token was refused".into());
    }
    if current == Some(label) {
        bits.push("runs start here".into());
    }
    bits.join(" · ")
}
