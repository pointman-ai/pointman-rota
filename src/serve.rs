//! `rota serve`: the extension its node runs. The node asks it which account each Claude Code or Codex
//! run starts on (`session.prepare`), tells it how each run ended (`session.ended`), and shows its
//! accounts as parts in `pointman health`; people and agents steer it with its commands.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::bail;
use pointman_extension::{Extension, Handle, Job};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::accounts::{self, Seen, DEFAULT};
use crate::signin::SignIns;
use crate::util::{self, now, or_default, text, truthy};
use crate::{codex, probe};

/// The last swaps, for the machine's status.
const MOVES_KEPT: usize = 10;
/// How often the vault's rota.* names are looked at.
const LIST_EVERY: Duration = Duration::from_secs(60);
/// How long an opened token is kept before it's opened again (each open is logged).
const OPEN_EVERY: Duration = Duration::from_secs(1800);

type Tokens = Vec<(String, Option<String>)>;
/// A method or command: what it answers, from its params.
type Answer = fn(&Rota, &Value) -> anyhow::Result<Value>;

/// One swap: when, from which account to which, and why.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Move {
    at: f64,
    from: Option<String>,
    to: String,
    why: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct CodexState {
    #[serde(default, deserialize_with = "or_default")]
    seen: BTreeMap<String, Seen>,
    #[serde(default)]
    current: Option<String>,
}

/// What rota keeps in `<state_dir>/rota.json` (no tokens): the same file as the Python rota's.
#[derive(Serialize, Deserialize, Default, Debug)]
struct State {
    #[serde(default, deserialize_with = "or_default")]
    seen: BTreeMap<String, Seen>,
    #[serde(default)]
    current: Option<String>,
    #[serde(default, deserialize_with = "or_default")]
    folders: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "or_default")]
    agents: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "or_default")]
    left: BTreeMap<String, f64>,
    #[serde(default, deserialize_with = "or_default")]
    moves: Vec<Move>,
    #[serde(default, deserialize_with = "or_default")]
    codex: CodexState,
}

/// The vault's rota.* items as this machine last saw them.
#[derive(Default)]
struct Vault {
    listed: Option<Instant>,
    names: Vec<String>,
    opened: HashMap<String, (Instant, Option<String>)>,
    said: Option<String>,
}

pub struct Rota {
    h: Handle,
    state: Mutex<State>,
    /// Whether the state has been read from its file: at start, or by whatever touches it first (a check
    /// can beat on_start there, and mustn't write over the file before it's read)
    loaded: AtomicBool,
    vault: Mutex<Vault>,
    signins: SignIns,
}

fn token_of<'a>(toks: &'a Tokens, label: &str) -> Option<&'a Option<String>> {
    toks.iter().find(|(l, _)| l == label).map(|(_, t)| t)
}

/// A run's account, and its token in the run's environment (none for this machine's own login).
fn run_on(label: &str, token: &Option<String>) -> Value {
    let env = match token {
        Some(t) => json!({"CLAUDE_CODE_OAUTH_TOKEN": t}),
        None => json!({}),
    };
    json!({"env": env, "account": label})
}

/// Whether an account can take a run at all: its token wasn't refused and it isn't full.
fn takes_runs(seen: Option<&Seen>, now: Option<f64>) -> bool {
    !seen.is_some_and(Seen::refused) && accounts::used(seen, now).unwrap_or(0.0) < 100.0
}

/// Why runs moved off an account, in a few words.
fn why_left(label: Option<&str>, seen: Option<&Seen>) -> String {
    let Some(label) = label else { return "first pick".into() };
    if seen.is_some_and(Seen::refused) {
        return format!("{label}'s token was refused");
    }
    match accounts::used(seen, None) {
        Some(full) => format!("{label} was at {full:.0}%"),
        None => format!("{label} had no room known"),
    }
}

fn folder_key(folder: Option<&Value>) -> Option<String> {
    let folder = text(folder);
    (!folder.is_empty()).then(|| util::resolve(&util::expand(&folder)).to_string_lossy().into_owned())
}

fn label_of(p: &Value, key: &str) -> anyhow::Result<String> {
    let label = text(p.get(key)).trim().to_lowercase();
    if !accounts::is_label(&label) {
        bail!("{key}: an account's label, like work");
    }
    Ok(label)
}

/// How a part is doing in `pointman health`, from how full it is and whether its token was refused.
fn part_state(seen: &Seen, full: Option<f64>) -> &'static str {
    match full {
        _ if seen.refused() => "failing",
        Some(f) if f >= 100.0 => "failing",
        None => "unknown",
        Some(_) => "ok",
    }
}

/// This program, as a person on this machine would type it (`~/…/bin/rota`).
fn this_program() -> String {
    let me = std::env::current_exe().map_or_else(|_| "rota".into(), |p| p.to_string_lossy().into_owned());
    match util::env("HOME").and_then(|h| me.strip_prefix(&format!("{h}/")).map(String::from)) {
        Some(rest) => format!("~/{rest}"),
        None => me,
    }
}

impl Rota {
    pub fn new(h: Handle) -> Self {
        Rota {
            h,
            state: Mutex::default(),
            loaded: AtomicBool::new(false),
            vault: Mutex::default(),
            signins: SignIns::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.loaded.swap(true, Ordering::SeqCst) {
            *st = self.load_state();
        }
        st
    }

    fn machine(&self) -> String {
        text(self.h.node().get("machine"))
    }

    /// The accounts kept in the board's vault as `rota.<label>`, for this machine: their names listed and
    /// each token opened through the node, which only lets rota see rota.* (MB-106).
    fn vault_tokens(&self) -> Vec<(String, String)> {
        let names = {
            let mut v = self.vault.lock().unwrap_or_else(|e| e.into_inner());
            if v.listed.is_none_or(|t| t.elapsed() > LIST_EVERY) {
                v.listed = Some(Instant::now());
                match self.h.request("secret.list", json!({"prefix": "rota."})) {
                    Ok(got) => {
                        v.names = got.get("names").and_then(Value::as_array).map_or_else(Vec::new, |names| {
                            names.iter().filter_map(Value::as_str).map(String::from).collect()
                        });
                        v.said = None;
                    }
                    // an older node or core: the local file only
                    Err(e) => {
                        let e = e.to_string();
                        if v.said.as_ref() != Some(&e) {
                            self.h.log(format!("no accounts from the vault: {e}"));
                            v.said = Some(e);
                        }
                    }
                }
            }
            v.names.clone()
        };
        let mut out = vec![];
        for name in names {
            let Some(label) = name.strip_prefix("rota.").map(str::to_lowercase) else { continue };
            if label == DEFAULT || !accounts::is_label(&label) {
                continue;
            }
            let kept = self.vault.lock().unwrap_or_else(|e| e.into_inner()).opened.get(&name).cloned();
            let token = match kept {
                Some((at, token)) if at.elapsed() <= OPEN_EVERY => token,
                _ => match self.h.secret(&name) {
                    Ok(value) => {
                        let token = accounts::find_token(&value);
                        let mut v = self.vault.lock().unwrap_or_else(|e| e.into_inner());
                        v.opened.insert(name.clone(), (Instant::now(), token.clone()));
                        token
                    }
                    Err(e) => {
                        self.h.log(format!("couldn't open {name}: {e}"));
                        continue;
                    }
                },
            };
            if let Some(token) = token {
                out.push((label, token));
            }
        }
        out
    }

    /// Every account runs may start on: this machine's own login, the vault's, and any in the local file
    /// (the vault's win for the same label); none that are held.
    fn all_tokens(&self) -> Tokens {
        let store = accounts::load_store(None);
        let mut out = accounts::tokens(&store);
        for (label, token) in self.vault_tokens() {
            if !store.accounts.iter().any(|a| a.held() && a.label == label) {
                accounts::put(&mut out, &label, Some(token));
            }
        }
        out
    }

    /// Accounts held out (`rota.hold`) that have a token somewhere: listed with `held`, never picked, so
    /// the apps keep a row to put one back from (loupe, thread 2255 [3517]).
    fn held_labels(&self, toks: &Tokens) -> Vec<String> {
        let store = accounts::load_store(None);
        let vault: Vec<String> = self.vault_tokens().into_iter().map(|(l, _)| l).collect();
        let have = |label: &String| {
            vault.contains(label) || store.accounts.iter().any(|a| &a.label == label && a.token.is_some())
        };
        store
            .accounts
            .iter()
            .filter(|a| a.held() && have(&a.label) && token_of(toks, &a.label).is_none())
            .map(|a| a.label.clone())
            .collect()
    }

    fn state_file(&self) -> PathBuf {
        let dir = text(self.h.node().get("state_dir"));
        PathBuf::from(if dir.is_empty() { ".".into() } else { dir }).join("rota.json")
    }

    fn load_state(&self) -> State {
        fs::read_to_string(self.state_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save_state(&self, state: &State) {
        let path = self.state_file();
        let tmp = path.with_file_name("rota.json.tmp");
        let saved = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|_| fs::write(&tmp, serde_json::to_string(state)?))
            .and_then(|_| fs::rename(&tmp, &path));
        if let Err(e) = saved {
            self.h.log(format!("couldn't keep rota's state in {}: {e}", path.display()));
        }
    }

    fn setting(&self, key: &str) -> Option<f64> {
        self.h.settings().get(key).and_then(Value::as_f64)
    }

    fn threshold(&self) -> f64 {
        self.setting("threshold").filter(|t| *t != 0.0).unwrap_or(90.0)
    }

    /// Seconds an account runs moved off stays out of the running (cswap's cooldown): its numbers can lag
    /// behind, and two nearly full accounts would otherwise take turns run by run.
    fn cooldown(&self) -> f64 {
        60.0 * self.setting("cooldown_minutes").unwrap_or(10.0)
    }

    /// A swap: kept for the machine's status (Machines, `pointman health`, rota.status) and logged.
    fn moved(&self, st: &mut State, was: Option<String>, to: &str, why: String) {
        let now = now();
        if let Some(was) = &was {
            st.left.insert(was.clone(), now);
        }
        self.h.log(format!("runs start on {to} now (was {}): {why}", was.as_deref().unwrap_or("None")));
        st.moves.push(Move { at: now, from: was, to: to.to_string(), why });
        let over = st.moves.len().saturating_sub(MOVES_KEPT);
        st.moves.drain(..over);
    }

    /// What the apps and agents see: labels, how full, when each resets, and which are held. Never a token.
    pub fn summary(&self) -> Value {
        let toks = self.all_tokens();
        let held = self.held_labels(&toks);
        let store = accounts::load_store(None);
        let codex_labels = codex::labels(&store);
        let now = now();
        let st = self.state();
        let current = st.current.as_deref();
        let rows: Vec<Value> = toks
            .iter()
            .map(|(label, _)| (label, false))
            .chain(held.iter().map(|label| (label, true)))
            .map(|(label, held)| {
                let seen = st.seen.get(label);
                let mut row = json!({"label": label, "own_login": label == DEFAULT, "held": held,
                                     "used": accounts::used(seen, Some(now))});
                if let Some(Value::Object(known)) = seen.map(|s| serde_json::to_value(s).unwrap_or_default()) {
                    for (k, v) in known {
                        if ["five_hour", "seven_day", "status", "needs_auth", "checked_at"].contains(&k.as_str()) {
                            row[k] = v;
                        }
                    }
                }
                let line = accounts::describe(label, seen, current, Some(now));
                row["line"] = json!(if held { line + " · held: runs stay off it" } else { line });
                row
            })
            .collect();
        let cx = &st.codex;
        let codex_rows: Vec<Value> = codex_labels
            .iter()
            .map(|label| {
                let seen = cx.seen.get(label);
                json!({"label": label, "own_login": label == DEFAULT, "used": accounts::used(seen, Some(now)),
                       "line": accounts::describe(label, seen, cx.current.as_deref(), Some(now))})
            })
            .collect();
        json!({"current": st.current, "threshold": self.threshold(), "folders": st.folders, "agents": st.agents,
               "moves": st.moves, "accounts": rows, "codex": {"current": cx.current, "accounts": codex_rows}})
    }

    fn tell(&self) {
        self.h.event("rota.accounts", self.summary());
    }

    /// A summary with a message for the person who asked.
    fn answer(&self, message: String) -> Value {
        let mut out = self.summary();
        out["message"] = json!(message);
        out
    }

    fn note(&self, label: &str, limits: Option<&Value>, error: Option<String>, refused: bool) {
        {
            let mut st = self.state();
            let seen = st.seen.entry(label.to_string()).or_default();
            if truthy(limits) {
                let got = accounts::windows(limits);
                seen.five_hour = got.five_hour;
                seen.seven_day = got.seven_day;
                seen.status = got.status;
                seen.blocked_until = got.blocked_until;
                seen.needs_auth = Some(false);
            } else if refused {
                seen.needs_auth = Some(true);
            }
            seen.checked_at = Some(now());
            seen.error = error;
            self.save_state(&st);
        }
        self.tell();
    }

    fn started(&self) {
        drop(self.state());
        self.tell();
    }

    /// The account a run starts on: its token goes in the run's environment, and none for this machine's
    /// own login.
    fn prepare(&self, p: &Value) -> anyhow::Result<Value> {
        match p.get("client").and_then(Value::as_str) {
            Some("codex") => return Ok(self.prepare_codex()),
            Some("claude") => {}
            _ => return Ok(json!({})),
        }
        let toks = self.all_tokens();
        let (label, moved) = {
            let mut st = self.state();
            // an agent mapped to an account, else its folder, while that account can take runs (cswap's map)
            let pins =
                [st.agents.get(&text(p.get("handle"))), folder_key(p.get("folder")).and_then(|k| st.folders.get(&k))];
            for pinned in pins.into_iter().flatten() {
                if let Some(token) = token_of(&toks, pinned).filter(|_| takes_runs(st.seen.get(pinned), None)) {
                    return Ok(run_on(pinned, token));
                }
            }
            let (current, now, cooldown) = (st.current.clone(), now(), self.cooldown());
            // one runs moved off lately sits out the cooldown, and one that can't take runs is out too; with
            // nothing left that can, every account is in the running again
            let labels: Vec<String> = toks.iter().map(|(l, _)| l.clone()).collect();
            let pool: Vec<String> = labels
                .iter()
                .filter(|l| {
                    let rested = now - st.left.get(*l).copied().unwrap_or(0.0) >= cooldown;
                    (rested || current.as_ref() == Some(*l)) && takes_runs(st.seen.get(*l), Some(now))
                })
                .cloned()
                .collect();
            let pool = if pool.is_empty() { labels } else { pool };
            let label = accounts::choose(&pool, &st.seen, current.as_deref(), self.threshold(), None);
            let moved = current.as_deref() != Some(label.as_str());
            if moved {
                let why = why_left(current.as_deref(), current.as_ref().and_then(|c| st.seen.get(c)));
                self.moved(&mut st, current, &label, why);
                st.current = Some(label.clone());
                self.save_state(&st);
            }
            (label, moved)
        };
        if moved {
            self.tell();
        }
        Ok(run_on(&label, token_of(&toks, &label).unwrap_or(&None)))
    }

    /// A Codex run's account: its own Codex home (CODEX_HOME), or nothing for the machine's own login.
    fn prepare_codex(&self) -> Value {
        let mut labels = codex::labels(&accounts::load_store(None));
        if labels.is_empty() {
            labels.push(DEFAULT.into());
        }
        let label = {
            let mut st = self.state();
            let cx = &mut st.codex;
            let label = accounts::choose(&labels, &cx.seen, cx.current.as_deref(), self.threshold(), None);
            if cx.current.as_deref() != Some(label.as_str()) {
                let was = cx.current.replace(label.clone());
                self.h.log(format!("Codex runs start on {label} now (was {})", was.as_deref().unwrap_or("None")));
                self.save_state(&st);
            }
            label
        };
        let env = if label == DEFAULT { json!({}) } else { json!({"CODEX_HOME": codex::home(&label)}) };
        json!({"env": env, "account": label})
    }

    /// A run's account as Claude Code last reported it, and whether its token was refused. A Codex run's
    /// from the newest session Codex wrote in that account's home.
    fn ended(&self, p: &Value) -> anyhow::Result<Value> {
        let label = Some(text(p.get("account"))).filter(|l| !l.is_empty()).unwrap_or_else(|| DEFAULT.into());
        let error = p.get("error").and_then(Value::as_str).map(String::from);
        if p.get("client").and_then(Value::as_str) == Some("codex") {
            let got = codex::latest(&label);
            {
                let mut st = self.state();
                let seen = st.codex.seen.entry(label).or_default();
                if let Some(got) = got {
                    seen.five_hour = got.five_hour;
                    seen.seven_day = got.seven_day;
                    seen.needs_auth = Some(false);
                } else if probe::refused(error.as_deref()) {
                    seen.needs_auth = Some(true);
                }
                seen.checked_at = Some(now());
                self.save_state(&st);
            }
            self.tell();
            return Ok(json!({}));
        }
        let limits = p.get("limits");
        let refused = probe::refused(error.as_deref()) && !truthy(limits);
        self.note(&label, limits, error, refused);
        Ok(json!({}))
    }

    /// Each account as a part under rota in `pointman health` and the apps' Machines page.
    fn health(&self) -> Value {
        let toks = self.all_tokens();
        let held = self.held_labels(&toks);
        let codex_labels = codex::labels(&accounts::load_store(None));
        let now = now();
        let st = self.state();
        let mut out = vec![];
        for (label, _) in &toks {
            let seen = st.seen.get(label).cloned().unwrap_or_default();
            let full = accounts::used(Some(&seen), Some(now));
            let mut why = accounts::describe(label, Some(&seen), st.current.as_deref(), Some(now));
            if let Some(last) = st.moves.last() {
                if st.current.as_ref() == Some(label) && &last.to == label && last.from.is_some() {
                    why += &format!(" since {} ({})", util::local_time(last.at, "%H:%M"), last.why);
                }
            }
            out.push(json!({"id": label, "kind": "account", "name": format!("Claude: {label}"),
                            "state": part_state(&seen, full), "needs": if seen.refused() { vec!["auth"] } else { vec![] },
                            "why": why}));
        }
        for label in held {
            out.push(json!({"id": label, "kind": "account", "name": format!("Claude: {label}"), "state": "stopped",
                            "needs": [], "why": "held: runs stay off it until it's put back (rota.hold with held false)"}));
        }
        for label in codex_labels {
            let seen = st.codex.seen.get(&label).cloned().unwrap_or_default();
            let full = accounts::used(Some(&seen), Some(now));
            out.push(json!({"id": format!("codex.{label}"), "kind": "account", "name": format!("Codex: {label}"),
                            "state": part_state(&seen, full), "needs": if seen.refused() { vec!["auth"] } else { vec![] },
                            "why": accounts::describe(&label, Some(&seen), st.codex.current.as_deref(), Some(now))}));
        }
        Value::Array(out)
    }

    fn fix(&self, p: &Value) -> anyhow::Result<Value> {
        let label = text(p.get("part"));
        let auth = p.get("action").and_then(Value::as_str) == Some("auth");
        if let Some(cx) = label.strip_prefix("codex.").filter(|_| auth) {
            return Ok(json!({"message": format!("On {}: run `{} codex add {cx}` and sign in again",
                                                self.machine(), this_program())}));
        }
        if !auth || label == DEFAULT {
            bail!("rota can only say how to sign an added account in again");
        }
        Ok(json!({"message": format!("On {}: run `claude setup-token` signed in as {label}'s account, \
                                      then `{} add {label}` and paste the token", self.machine(), this_program())}))
    }

    /// Sign an account in from the apps (MB-216): Claude Code's own setup-token, its link for the person.
    fn sign_in(&self, p: &Value) -> anyhow::Result<Value> {
        if p.get("form").and_then(Value::as_str) != Some("account") {
            bail!("rota signs in Claude accounts only (form: account)");
        }
        label_of(p, "label")?;
        self.signins.start(&text(p.get("session")), Duration::from_secs(60))
    }

    /// The code the sign-in page showed: the token comes back for the node to seal into the vault.
    fn sign_in_code(&self, p: &Value) -> anyhow::Result<Value> {
        let dir = text(self.h.node().get("state_dir"));
        let keep = (!dir.is_empty()).then(|| PathBuf::from(dir));
        self.signins.code(&text(p.get("session")), &text(p.get("code")), Duration::from_secs(120), keep.as_deref())
    }

    /// New runs start on this account from now on (or, with none named, the one with the most room), and
    /// stay there until it's full, as after any move.
    fn swap(&self, params: &Value) -> anyhow::Result<Value> {
        let toks = self.all_tokens();
        let held = self.held_labels(&toks);
        let was = {
            let mut st = self.state();
            let labels: Vec<String> = toks.iter().map(|(l, _)| l.clone()).collect();
            let to = if truthy(params.get("to")) {
                label_of(params, "to")?
            } else {
                accounts::most_room(&labels, &st.seen, None).unwrap_or_default()
            };
            if held.contains(&to) {
                bail!("{to} is held: put it back first (rota.hold with held false)");
            }
            if token_of(&toks, &to).is_none() {
                bail!("no account '{to}' to start runs on here: {}", labels.join(", "));
            }
            if st.seen.get(&to).is_some_and(Seen::refused) {
                bail!("{to}'s token was refused: sign it in again first");
            }
            let was = st.current.replace(to.clone());
            if was.as_ref() != Some(&to) {
                self.moved(&mut st, was.clone(), &to, "asked".into());
            }
            self.save_state(&st);
            (was, to)
        };
        self.tell();
        let (was, to) = was;
        let since = was.map_or(String::new(), |was| format!(" (was {was})"));
        Ok(self.answer(format!("New runs on {} start on {to}{since}", self.machine())))
    }

    /// Keep runs off an account until it's put back (`use`): its token stays where it is.
    fn hold(&self, params: &Value) -> anyhow::Result<Value> {
        let label = label_of(params, "account")?;
        if label == DEFAULT {
            bail!("the machine's own login can't be held: it's what runs fall back to");
        }
        let held = params.get("held") != Some(&Value::Bool(false));
        accounts::hold(&label, held, None)?;
        self.tell();
        Ok(self.answer(if held { format!("runs stay off {label}") } else { format!("{label} is back in turn") }))
    }

    /// An agent's runs start on this account while it has room (none named: back to the rule).
    fn agent(&self, params: &Value) -> anyhow::Result<Value> {
        let handle = text(params.get("agent")).trim().trim_start_matches('@').to_lowercase();
        let short: String = handle.replace('-', "_").chars().take(32).collect();
        if handle.is_empty() || !accounts::is_label(&short) {
            bail!("agent: its handle, like scene");
        }
        let label = if truthy(params.get("account")) { Some(label_of(params, "account")?) } else { None };
        {
            let mut st = self.state();
            match &label {
                Some(label) => st.agents.insert(handle.clone(), label.clone()),
                None => st.agents.remove(&handle),
            };
            self.save_state(&st);
        }
        self.tell();
        Ok(self.answer(match label {
            Some(label) => format!("@{handle}'s runs start on {label}"),
            None => format!("@{handle}'s runs follow the rule again"),
        }))
    }

    /// Runs in this folder start on this account while it has room (none named: back to the rule).
    fn folder(&self, params: &Value) -> anyhow::Result<Value> {
        let Some(key) = folder_key(params.get("folder")) else {
            bail!("folder: an agent's folder on this machine");
        };
        let label = if truthy(params.get("account")) { Some(label_of(params, "account")?) } else { None };
        {
            let mut st = self.state();
            match &label {
                Some(label) => st.folders.insert(key.clone(), label.clone()),
                None => st.folders.remove(&key),
            };
            self.save_state(&st);
        }
        self.tell();
        Ok(self.answer(match label {
            Some(label) => format!("runs in {key} start on {label}"),
            None => format!("runs in {key} follow the rule again"),
        }))
    }

    /// Check each account nothing has told rota about lately, one at a time.
    fn check_idle(&self) {
        let every = 60.0 * self.setting("check_minutes").filter(|m| *m != 0.0).unwrap_or(30.0);
        for (label, token) in self.all_tokens() {
            let checked = self.state().seen.get(&label).and_then(|s| s.checked_at).unwrap_or(0.0);
            if now() - checked < every {
                continue;
            }
            match probe::check(token.as_deref(), None, Duration::from_secs(120)) {
                Ok(got) => self.note(&label, got.limits.as_ref(), got.error, got.refused),
                Err(e) => self.h.log(format!("couldn't check {label}: {e}")),
            }
        }
    }
}

pub fn serve() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let rota = Arc::new(Rota::new(ext.handle()));
    let r = rota.clone();
    ext.on_start(move |_: &Handle| r.started());

    let methods: [(&str, Answer); 6] = [
        ("session.prepare", Rota::prepare),
        ("session.ended", Rota::ended),
        ("health.check", |r, _| Ok(r.health())),
        ("health.fix", Rota::fix),
        ("credentials.sign_in", Rota::sign_in),
        ("credentials.code", Rota::sign_in_code),
    ];
    for (name, f) in methods {
        let r = rota.clone();
        ext.method(name, move |p: Value| f(&r, &p));
    }
    let commands: [(&str, Answer); 5] = [
        ("rota.status", |r, _| Ok(r.summary())),
        ("rota.swap", Rota::swap),
        ("rota.hold", Rota::hold),
        ("rota.agent", Rota::agent),
        ("rota.folder", Rota::folder),
    ];
    for (kind, f) in commands {
        let r = rota.clone();
        ext.command(kind, move |job: &Job| f(&r, &job.params));
    }
    let r = rota.clone();
    ext.every(Duration::from_secs(60), move |_: &Handle| r.check_idle(), "checks");
    ext.run()
}
