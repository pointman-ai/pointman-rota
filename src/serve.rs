//! `rota serve`: the extension its node runs. The node asks it which account each Claude Code or Codex
//! run starts on (`session.prepare`), tells it how each run ended (`session.ended`), and shows its
//! accounts as parts in `pointman health`; people and agents steer it with its commands.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::bail;
use pointman_extension::{Extension, Handle, Job, Pane, Refused};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::accounts::{self, Seen, DEFAULT};
use crate::signin::SignIns;
use crate::util::{self, now, or_default, text, truthy};
use crate::{codex, probe};

/// The last swaps, for the machine's status.
const MOVES_KEPT: usize = 10;
/// How often the vault's rota.* names are looked at. A list older than this is still used while it's
/// asked for again behind (each ask is a round trip to core, 0.2 s from the Mac, that a run's start waited
/// on: Pointman thread 6216); one older than OPEN_EVERY is asked for first.
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
    /// The names accounts are shown and typed by, where someone renamed one: its label → the name
    /// (`/rota rename default james-max`; James, Pointman thread 2255 [4136]). Labels stay as they are,
    /// so vault items, health ids and the runs' accounts don't change.
    #[serde(default, deserialize_with = "or_default", skip_serializing_if = "BTreeMap::is_empty")]
    names: BTreeMap<String, String>,
}

impl State {
    /// An account as it's shown: the name it was given, else its label.
    fn shown(&self, label: &str) -> String {
        self.names.get(label).cloned().unwrap_or_else(|| label.to_string())
    }

    /// The label a name someone typed means: the account given that name, else the name as a label.
    fn label(&self, name: String) -> String {
        self.names.iter().find(|(_, n)| **n == name).map(|(l, _)| l.clone()).unwrap_or(name)
    }
}

/// The vault's rota.* items as this machine last saw them.
#[derive(Default)]
struct Vault {
    listed: Option<Instant>,
    names: Vec<String>,
    /// The list being asked for behind.
    asking: Option<Receiver<Result<Vec<String>, String>>>,
    opened: HashMap<String, (Instant, Option<String>)>,
    said: Option<String>,
}

/// What's done about the vault's list, from its age and whether it's being asked for already.
#[derive(Debug, PartialEq)]
enum Relist {
    Now,
    Behind,
    Kept,
}

fn relist(age: Option<Duration>, asking: bool) -> Relist {
    match age {
        None => Relist::Now,
        Some(age) if age > OPEN_EVERY => Relist::Now,
        Some(age) if age > LIST_EVERY && !asking => Relist::Behind,
        Some(_) => Relist::Kept,
    }
}

/// The vault's rota.* names, as the node lists them.
fn list(h: &Handle) -> Result<Vec<String>, String> {
    let got = h.request("secret.list", json!({"prefix": "rota."})).map_err(|e| e.to_string())?;
    Ok(got
        .get("names")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |names| names.iter().filter_map(Value::as_str).map(String::from).collect()))
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

/// Whether a run was granted claude.ai connectors (the node passes their names as `connectors`): only this
/// machine's own login loads them.
fn has_connectors(p: &Value) -> bool {
    p.get("connectors")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|v| v.as_str().is_some_and(|s| !s.is_empty())))
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
/// A summary with `text` and `blocks` (view.rs) beside its own fields, which MB-44's view reads.
fn with_view(mut out: Value, message: Option<&str>) -> Value {
    let (text, blocks) = crate::view::view(&out, message);
    out["text"] = json!(text);
    out["blocks"] = blocks;
    out
}

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
            let mut got = None;
            if let Some(asking) = &v.asking {
                match asking.try_recv() {
                    Ok(answer) => got = Some(answer),
                    Err(TryRecvError::Disconnected) => v.asking = None,
                    Err(TryRecvError::Empty) => {}
                }
            }
            if got.is_some() {
                v.asking = None;
            }
            match relist(v.listed.map(|t| t.elapsed()), v.asking.is_some()) {
                Relist::Now => {
                    v.listed = Some(Instant::now());
                    v.asking = None;
                    got = Some(list(&self.h));
                }
                Relist::Behind => {
                    v.listed = Some(Instant::now());
                    let (tx, rx) = mpsc::channel();
                    v.asking = Some(rx);
                    let h = self.h.clone();
                    std::thread::spawn(move || tx.send(list(&h)));
                }
                Relist::Kept => {}
            }
            match got {
                Some(Ok(names)) => {
                    v.names = names;
                    v.said = None;
                }
                // an older node or core: the local file only
                Some(Err(e)) => {
                    if v.said.as_ref() != Some(&e) {
                        self.h.log(format!("no accounts from the vault: {e}"));
                        v.said = Some(e);
                    }
                }
                None => {}
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
    /// (the vault's win for the same label).
    fn all_tokens(&self) -> Tokens {
        let store = accounts::load_store(None);
        let mut out = accounts::tokens(&store);
        for (label, token) in self.vault_tokens() {
            accounts::put(&mut out, &label, Some(token));
        }
        out
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

    /// What the apps and agents see: each account by the name it's shown by (`label`, and its own label as
    /// `id`), how full, when each resets. Never a token.
    pub fn summary(&self) -> Value {
        let toks = self.all_tokens();
        let store = accounts::load_store(None);
        let codex_labels = codex::labels(&store);
        let now = now();
        let st = self.state();
        let current = st.current.as_deref();
        let rows: Vec<Value> = toks
            .iter()
            .map(|(label, _)| {
                let seen = st.seen.get(label);
                let mut row = json!({"label": st.shown(label), "id": label, "own_login": label == DEFAULT,
                                     "used": accounts::used(seen, Some(now))});
                if let Some(Value::Object(known)) = seen.map(|s| serde_json::to_value(s).unwrap_or_default()) {
                    for (k, v) in known {
                        if ["five_hour", "seven_day", "status", "needs_auth", "checked_at", "extra", "credits"]
                            .contains(&k.as_str())
                        {
                            row[k] = v;
                        }
                    }
                }
                row["line"] = json!(accounts::describe(label, seen, current, Some(now)));
                row
            })
            .collect();
        let cx = &st.codex;
        let codex_rows: Vec<Value> = codex_labels
            .iter()
            .map(|label| {
                let seen = cx.seen.get(label);
                let mut row = json!({"label": label, "id": format!("codex.{label}"), "own_login": label == DEFAULT,
                                     "used": accounts::used(seen, Some(now))});
                if let Some(Value::Object(known)) = seen.map(|s| serde_json::to_value(s).unwrap_or_default()) {
                    for (k, v) in known {
                        if ["five_hour", "seven_day", "status", "needs_auth", "checked_at", "codex"]
                            .contains(&k.as_str())
                        {
                            row[k] = v;
                        }
                    }
                }
                // only the machine's own Codex runs pick accounts today: "runs start here" says nothing new
                row["line"] = json!(accounts::describe(label, seen, None, Some(now)));
                row
            })
            .collect();
        let named = |m: &BTreeMap<String, String>| -> BTreeMap<String, String> {
            m.iter().map(|(k, label)| (k.clone(), st.shown(label))).collect()
        };
        let moves: Vec<Move> = st
            .moves
            .iter()
            .map(|m| Move { from: m.from.as_deref().map(|l| st.shown(l)), to: st.shown(&m.to), ..m.clone() })
            .collect();
        json!({"current": current.map(|c| st.shown(c)), "threshold": self.threshold(), "folders": named(&st.folders),
               "agents": named(&st.agents), "moves": moves, "accounts": rows,
               "codex": {"current": cx.current, "accounts": codex_rows}})
    }

    /// What changed goes to the apps: the event (only when it did), and each open accounts pane, whose
    /// show sends only what changed.
    fn tell(&self) {
        let summary = self.summary();
        for p in self.h.panes("accounts") {
            p.show(crate::view::pane(&summary, now()));
        }
        self.h.event("rota.accounts", summary);
    }

    /// A tap on the accounts pane's buttons: `swap` (the one with the most room), or `use:<label>`.
    fn tapped(&self, event: &Value) -> anyhow::Result<()> {
        let button = event["value"].as_str().unwrap_or_default();
        if event["kind"] != "tap" || event["block"] != "actions" {
            bail!(Refused::new("only the buttons do something here"));
        }
        let params = match button.strip_prefix("use:") {
            Some(label) => json!({"to": label}),
            None if button == "swap" => json!({}),
            None => bail!(Refused::new(format!("no button {button}"))),
        };
        self.swap(&params).map(drop).map_err(|e| Refused::new(format!("{e:#}")).into())
    }

    /// A summary with a message for the person who asked, and what the apps draw for it: a button on
    /// `/rota usage` runs a swap, and its answer replaces that post's blocks (MB-276).
    fn answer(&self, message: String) -> Value {
        let mut out = self.summary();
        out["message"] = json!(message);
        with_view(out, Some(&message))
    }

    /// `/rota usage` and the telemetry panel (MB-276): the summary, with its text and blocks. Codex's
    /// accounts are asked first when their numbers are over a minute old: that takes under a second.
    pub fn usage(&self) -> Value {
        self.check_codex(60.0);
        with_view(self.summary(), None)
    }

    /// Ask Codex for the usage of each signed-in Codex account whose numbers are older than `every`
    /// seconds (James, Pointman thread 3359 [7694]), and tell the apps when any came in.
    fn check_codex(&self, every: f64) {
        let labels = codex::labels(&accounts::load_store(None));
        let mut told = false;
        for label in labels.into_iter().filter(|l| codex::signed_in(l)) {
            let checked = self.state().codex.seen.get(&label).and_then(|s| s.checked_at).unwrap_or(0.0);
            if now() - checked < every {
                continue;
            }
            let got = codex::check(&label, None, Duration::from_secs(30));
            let mut st = self.state();
            let seen = st.codex.seen.entry(label.clone()).or_default();
            match got {
                Ok(got) => {
                    seen.five_hour = got.five_hour;
                    seen.seven_day = got.seven_day;
                    seen.status = got.status;
                    seen.blocked_until = got.blocked_until;
                    seen.codex = got.codex;
                    seen.needs_auth = Some(false);
                    seen.error = None;
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    if seen.error.as_ref() != Some(&e) {
                        self.h.log(format!("couldn't ask Codex for {label}'s usage: {e}"));
                    }
                    seen.needs_auth = Some(probe::refused(Some(&e)));
                    seen.error = Some(e);
                }
            }
            seen.checked_at = Some(now());
            self.save_state(&st);
            told = true;
        }
        if told {
            self.tell();
        }
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
                seen.extra = got.extra.or(seen.extra.take());
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
            // while the account can take runs: an agent mapped to one (cswap's map); a run granted claude.ai
            // connectors, on this machine's own login, since a setup token loads none of them (Pointman thread
            // 1018 [4263]); the agent's folder's account
            let own = has_connectors(p).then(|| DEFAULT.to_string());
            let pins = [
                st.agents.get(&text(p.get("handle"))).cloned(),
                own,
                folder_key(p.get("folder")).and_then(|k| st.folders.get(&k)).cloned(),
            ];
            for pinned in pins.into_iter().flatten() {
                if let Some(token) = token_of(&toks, &pinned).filter(|_| takes_runs(st.seen.get(&pinned), None)) {
                    return Ok(run_on(&pinned, token));
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
            out.push(json!({"id": label, "kind": "account", "name": format!("Claude: {}", st.shown(label)),
                            "state": part_state(&seen, full), "needs": if seen.refused() { vec!["auth"] } else { vec![] },
                            "why": why}));
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

    /// `/rota add work` as a post (an agent's, or an app's from before a command could name its form): the
    /// apps open the account form for it, since a token or a sign-in code never goes in a post, so this
    /// says where to add one.
    fn add(&self, params: &Value) -> anyhow::Result<Value> {
        let name = if truthy(params.get("name")) { label_of(params, "name")? } else { "work".into() };
        let toks = self.all_tokens();
        let there = {
            let st = self.state();
            toks.iter().any(|(l, _)| *l == name || st.shown(l) == name)
        };
        let again =
            if there { format!("{name} is here already: adding it again signs it in again. ") } else { String::new() };
        Ok(self.answer(format!(
            "{again}A Claude account is added in the Pointman app, so its sign-in never goes in a post: type \
             `/rota add {name}` there and tap Sign in. Or on {}: `claude setup-token` signed in as the account, \
             then `{} add {name}` and paste the token.",
            self.machine(),
            this_program()
        )))
    }

    /// New runs start on this account from now on (or, with none named, the one with the most room), and
    /// stay there until it's full, as after any move.
    fn swap(&self, params: &Value) -> anyhow::Result<Value> {
        let toks = self.all_tokens();
        let was = {
            let mut st = self.state();
            let labels: Vec<String> = toks.iter().map(|(l, _)| l.clone()).collect();
            let to = if truthy(params.get("to")) {
                st.label(label_of(params, "to")?)
            } else {
                accounts::most_room(&labels, &st.seen, None).unwrap_or_default()
            };
            if token_of(&toks, &to).is_none() {
                let names: Vec<String> = labels.iter().map(|l| st.shown(l)).collect();
                bail!("no account '{}' to start runs on here: {}", st.shown(&to), names.join(", "));
            }
            if st.seen.get(&to).is_some_and(Seen::refused) {
                bail!("{}'s token was refused: sign it in again first", st.shown(&to));
            }
            let was = st.current.replace(to.clone());
            if was.as_ref() != Some(&to) {
                self.moved(&mut st, was.clone(), &to, "asked".into());
            }
            self.save_state(&st);
            (was.map(|w| st.shown(&w)), st.shown(&to))
        };
        self.tell();
        let (was, to) = was;
        let since = was.map_or(String::new(), |was| format!(" (was {was})"));
        Ok(self.answer(format!("New runs on {} start on {to}{since}", self.machine())))
    }

    /// Give an account the name it's shown and typed by (`/rota rename default james-max`; James, Pointman
    /// thread 2255 [4136]). Its label stays, so its vault item and what's kept about it don't change; the
    /// name its label had is free again once it has another.
    fn rename(&self, params: &Value) -> anyhow::Result<Value> {
        let toks = self.all_tokens();
        let from_name = label_of(params, "from")?;
        let to = label_of(params, "to")?;
        let (was, now) = {
            let mut st = self.state();
            let label = st.label(from_name.clone());
            if token_of(&toks, &label).is_none() {
                let names: Vec<String> = toks.iter().map(|(l, _)| st.shown(l)).collect();
                bail!("no account '{from_name}' here: {}", names.join(", "));
            }
            let taken = toks.iter().any(|(l, _)| l != &label && (st.shown(l) == to || *l == to));
            if taken {
                bail!("another account here is called {to}: pick another name");
            }
            let was = st.shown(&label);
            if to == label {
                st.names.remove(&label);
            } else {
                st.names.insert(label.clone(), to.clone());
            }
            self.save_state(&st);
            (was, st.shown(&label))
        };
        self.tell();
        Ok(self.answer(format!("{was} is called {now} now")))
    }

    /// An agent's runs start on this account while it has room (none named: back to the rule).
    fn agent(&self, params: &Value) -> anyhow::Result<Value> {
        let handle = text(params.get("agent")).trim().trim_start_matches('@').to_lowercase();
        let short: String = handle.replace('-', "_").chars().take(32).collect();
        if handle.is_empty() || !accounts::is_label(&short) {
            bail!("agent: its handle, like scene");
        }
        let named = if truthy(params.get("account")) { Some(label_of(params, "account")?) } else { None };
        let label = {
            let mut st = self.state();
            let label = named.map(|n| st.label(n));
            match &label {
                Some(label) => st.agents.insert(handle.clone(), label.clone()),
                None => st.agents.remove(&handle),
            };
            self.save_state(&st);
            label.map(|l| st.shown(&l))
        };
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
        let named = if truthy(params.get("account")) { Some(label_of(params, "account")?) } else { None };
        let label = {
            let mut st = self.state();
            let label = named.map(|n| st.label(n));
            match &label {
                Some(label) => st.folders.insert(key.clone(), label.clone()),
                None => st.folders.remove(&key),
            };
            self.save_state(&st);
            label.map(|l| st.shown(&l))
        };
        self.tell();
        Ok(self.answer(match label {
            Some(label) => format!("runs in {key} start on {label}"),
            None => format!("runs in {key} follow the rule again"),
        }))
    }

    /// An account's usage credits from Claude Code's `get_usage`, kept until it answers again. Told with the
    /// next `note`, or by the caller.
    fn note_usage(&self, label: &str, got: &probe::Checked) {
        let mut st = self.state();
        let seen = st.seen.entry(label.to_string()).or_default();
        seen.usage_at = Some(now());
        if let Some(credits) = accounts::credits(got.usage.as_ref(), now()) {
            seen.credits = Some(credits);
        } else if let Some(why) = &got.usage_error {
            self.h.log(format!("no usage for {label} from Claude Code: {why}"));
        }
        self.save_state(&st);
    }

    /// Check each account nothing has told rota about lately, one at a time, and ask Claude Code for the
    /// usage credits of those runs keep busy (that needs no turn).
    fn check_idle(&self) {
        let every = 60.0 * self.setting("check_minutes").filter(|m| *m != 0.0).unwrap_or(30.0);
        for (label, token) in self.all_tokens() {
            let (checked, asked, refused) = {
                let st = self.state();
                let seen = st.seen.get(&label);
                (
                    seen.and_then(|s| s.checked_at).unwrap_or(0.0),
                    seen.and_then(|s| s.usage_at).unwrap_or(0.0),
                    seen.is_some_and(Seen::refused),
                )
            };
            if now() - checked >= every {
                match probe::check(token.as_deref(), None, Duration::from_secs(120)) {
                    Ok(got) => {
                        self.note_usage(&label, &got);
                        self.note(&label, got.limits.as_ref(), got.error, got.refused)
                    }
                    Err(e) => self.h.log(format!("couldn't check {label}: {e}")),
                }
            } else if now() - asked >= every && !refused {
                match probe::usage(token.as_deref(), None, Duration::from_secs(60)) {
                    Ok(got) => {
                        self.note_usage(&label, &got);
                        self.tell();
                    }
                    Err(e) => self.h.log(format!("couldn't ask for {label}'s usage: {e}")),
                }
            }
        }
        self.check_codex(every);
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
    let commands: [(&str, Answer); 7] = [
        ("rota.status", |r, _| Ok(r.summary())),
        ("rota.usage", |r, _| Ok(r.usage())),
        ("rota.add", Rota::add),
        ("rota.swap", Rota::swap),
        ("rota.rename", Rota::rename),
        ("rota.agent", Rota::agent),
        ("rota.folder", Rota::folder),
    ];
    for (kind, f) in commands {
        let r = rota.clone();
        ext.command(kind, move |job: &Job| f(&r, &job.params));
    }
    // the accounts pane (Pointman's docs/dev/panes.md): kept current by tell()
    let r = rota.clone();
    ext.pane("accounts", move |p: &Pane| {
        let tapped = r.clone();
        p.on_input(move |_: &Pane, event: &Value| tapped.tapped(event));
        Ok(json!({"title": "Accounts", "blocks": crate::view::pane(&r.summary(), now())}))
    });
    let r = rota.clone();
    ext.every(Duration::from_secs(60), move |_: &Handle| r.check_idle(), "checks");
    ext.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vaults_list_is_asked_for_first_then_behind_and_first_again_when_too_old() {
        let secs = |s: u64| Some(Duration::from_secs(s));
        assert_eq!(relist(None, false), Relist::Now);
        assert_eq!(relist(secs(10), false), Relist::Kept);
        assert_eq!(relist(secs(61), false), Relist::Behind);
        assert_eq!(relist(secs(61), true), Relist::Kept, "asked for once");
        assert_eq!(relist(secs(1801), true), Relist::Now);
    }
}
