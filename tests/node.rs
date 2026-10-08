//! Rota under a stand-in node (the SDK's), with a stand-in for Claude Code on its PATH. The scenarios and
//! assertions are the Python rota's (0.6.1), one for one.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pointman_extension::testing::StandIn;
use rota::accounts::{self, Account, Store};
use rota::codex;
use serde_json::{json, Value};
use tempfile::TempDir;

fn work() -> String {
    format!("sk-ant-oat01-{}", "w".repeat(40))
}

fn spare() -> String {
    format!("sk-ant-oat01-{}", "s".repeat(40))
}

const FAKE: &str = r#"#!/bin/sh
if [ "$1" = "setup-token" ]; then
  # Claude Code's sign-in, as far as rota needs it: a link that comes back to this machine (it would open
  # a browser), one that works anywhere, then the token for the right code
  open "https://claude.com/cai/oauth/authorize?code=true&redirect_uri=http%3A%2F%2Flocalhost%3A1234%2Fcallback&state=x"
  printf 'Browser didn'"'"'t open? Use the url below to sign in\n'
  printf '\033]8;id=1;https://claude.com/cai/oauth/authorize?code=true&redirect_uri=https%%3A%%2F%%2Fplatform.claude.com%%2Foauth%%2Fcode%%2Fcallback&state=x\007link\033]8;;\007\n'
  printf 'Paste code here if prompted > '
  read code
  if [ "$code" = "good-code-1234" ]; then printf '\nYour OAuth token (valid for 1 year):\n\nsk-ant-oat01-%s\n' "signedinsignedinsignedinsignedin1234"
  else printf '\nOAuth error: Invalid code\n'; fi
  exit 0
fi
# Claude Code, as far as rota's check needs: one rate_limit_event, how full by which token it's given
case "$CLAUDE_CODE_OAUTH_TOKEN" in
  "") used=0.95 ;;
  *bad*) echo '{"type":"result","is_error":true,"result":"OAuth token has expired"}'; exit 1 ;;
  *) used=0.2 ;;
esac
echo "$CLAUDE_CODE_OAUTH_TOKEN" >> "$FAKE_CALLS"
echo '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":'$used',"resetsAt":4102444800},"seven_day":{"utilization":0.1,"resetsAt":4102444800}}}}'
echo '{"type":"result","is_error":false,"result":"OK."}'
"#;

/// Rota running under a stand-in node, with a stand-in for Claude Code on its PATH, two accounts in its
/// file (work, and old, whose token is refused) and one in the vault (spare).
struct Host {
    node: StandIn,
    tmp: TempDir,
}

impl Host {
    fn start() -> Host {
        Host::with_state(|_| {})
    }

    /// Started with whatever `before` leaves in its state folder.
    fn with_state(before: impl FnOnce(&Path)) -> Host {
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("claude"), FAKE).unwrap();
        fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        let store = tmp.path().join("rota.json");
        let bad = format!("sk-ant-oat01-{}", "bad".repeat(10));
        let account = |label: &str, token: String| Account { token: Some(token), ..Account::new(label) };
        let accounts = vec![account("work", work()), account("old", bad)];
        accounts::save_store(&Store { accounts, codex: vec![] }, Some(&store)).unwrap();
        let state = tmp.path().join("state");
        fs::create_dir(&state).unwrap();
        before(&state);
        let at = |p: &str| tmp.path().join(p).to_string_lossy().into_owned();
        // the board's vault, as the node shows it to rota: the names rota.* covers, and their values
        let node = StandIn::new(env!("CARGO_MANIFEST_DIR"))
            .binary(env!("CARGO_BIN_EXE_rota"))
            .secrets([("rota.spare", format!("CLAUDE_CODE_OAUTH_TOKEN={}\n", spare())), ("rota.Not A Label", spare())])
            .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
            .env("ROTA_STORE", &store)
            .env("FAKE_CALLS", at("calls"))
            .env("CODEX_HOME", at("codex-own"))
            .env("ROTA_CODEX_HOMES", at("codex-homes"))
            .node("studio")
            .machine("studio")
            .state_dir(&state)
            .start()
            .unwrap();
        Host { node, tmp }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.tmp.path().join(p)
    }

    fn call(&self, method: &str, params: Value) -> Value {
        self.node.call(method, params).unwrap_or_else(|e| panic!("{method}: {e}\n{}", self.node.log()))
    }

    fn fails(&self, method: &str, params: Value) -> String {
        match self.node.call_timeout(method, params, Duration::from_secs(60)) {
            Ok(got) => panic!("{method} didn't fail: {got}"),
            Err(e) => e.to_string(),
        }
    }

    fn run(&self, kind: &str, params: Value) -> Value {
        self.node.run(kind, params).unwrap_or_else(|e| panic!("{kind}: {e}\n{}", self.node.log())).result
    }

    fn refuses(&self, kind: &str, params: Value) -> String {
        match self.node.run(kind, params) {
            Ok(done) => panic!("{kind} didn't fail: {}", done.result),
            Err(e) => e.to_string(),
        }
    }

    fn account(&self, p: Value) -> String {
        self.call("session.prepare", p)["account"].as_str().unwrap().to_string()
    }

    fn ended(&self, account: &str, status: &str, error: Value, limits: Value) {
        self.call(
            "session.ended",
            json!({"run": 0, "account": account, "status": status, "error": error, "limits": limits}),
        );
    }

    /// The health parts by id, once `until` holds (or 30 s on).
    fn parts(&self, until: impl Fn(&BTreeMap<String, Value>) -> bool) -> BTreeMap<String, Value> {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let got: BTreeMap<String, Value> = self
                .call("health.check", json!({}))
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p["id"].as_str().unwrap().to_string(), p.clone()))
                .collect();
            if until(&got) || Instant::now() > end {
                return got;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn status(&self) -> Value {
        self.run("rota.status", json!({}))
    }
}

fn why(parts: &BTreeMap<String, Value>, id: &str) -> String {
    parts[id]["why"].as_str().unwrap_or_default().to_string()
}

/// Every Claude account checked once (a Codex account stays unknown until a run on it).
fn claude_checked(got: &BTreeMap<String, Value>) -> bool {
    got.iter().filter(|(id, _)| !id.starts_with("codex.")).all(|(_, p)| p["state"] != "unknown")
}

fn rows(status: &Value) -> BTreeMap<String, Value> {
    status["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["label"].as_str().unwrap().to_string(), a.clone()))
        .collect()
}

fn five_hour(used: f64) -> Value {
    json!({"status": "allowed", "unifiedWindows": {"five_hour": {"utilization": used, "resetsAt": 4102444800u64}}})
}

fn over() -> Value {
    json!({"status": "rejected", "resetsAt": 4102444800u64,
           "unifiedWindows": {"five_hour": {"utilization": 1.0, "resetsAt": 4102444800u64}}})
}

#[test]
fn rota_on_a_node() {
    let host = Host::start();
    // it checks each account once at start, through Claude Code, on each one's own token
    let got = host.parts(claude_checked);
    assert!(why(&got, "default").starts_with("5h 95%"), "{got:?}");
    assert!(why(&got, "work").starts_with("5h 20%") && got["work"]["state"] == "ok");
    assert_eq!(got["old"]["needs"], json!(["auth"]));
    assert_eq!(got["old"]["state"], "failing");
    // an account kept in the vault, as rota.spare, is one too; a name that isn't a label isn't
    assert!(why(&got, "spare").starts_with("5h 20%") && !format!("{got:?}").contains("Not A Label"));
    // the machine's own login is past 90%, so runs start on work, with its token
    let run = host.call("session.prepare", json!({"run": 1, "handle": "scene", "client": "claude"}));
    assert_eq!(run, json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": work()}, "account": "work"}));
    assert_eq!(host.call("session.prepare", json!({"run": 2, "handle": "scene", "client": "gemini"})), json!({}));
    // a run's own report moves it on: work fills up, so runs go to the vault's spare, with its token
    host.ended("work", "done", Value::Null, five_hour(0.97));
    assert_eq!(
        host.call("session.prepare", json!({"run": 3, "client": "claude"})),
        json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": spare()}, "account": "spare"})
    );
    // and spare full too: nothing else has room, so they stay put
    host.ended("spare", "done", Value::Null, five_hour(0.95));
    assert_eq!(host.account(json!({"run": 4, "client": "claude"})), "spare");
    let status = host.status();
    assert_eq!(status["current"], "spare");
    assert!(!status.to_string().contains(&work()));
    let order: Vec<&str> =
        status["accounts"].as_array().unwrap().iter().map(|a| a["label"].as_str().unwrap()).collect();
    assert_eq!(order, ["default", "work", "old", "spare"]);
    assert!(!status.to_string().contains(&spare()));
    let fixed = host.call("health.fix", json!({"part": "old", "action": "auth"}));
    let message = fixed["message"].as_str().unwrap();
    assert!(message.contains("claude setup-token") && message.contains("rota add old"), "{message}");
}

#[test]
fn swapping_renaming_and_folders() {
    let host = Host::start();
    host.parts(claude_checked);
    assert_eq!(host.account(json!({"run": 1, "client": "claude"})), "work");
    // asked to, runs move at once, and stay there as after any move
    let got = host.run("rota.swap", json!({"to": "spare"}));
    assert_eq!(got["current"], "spare");
    assert_eq!(got["message"], "New runs on studio start on spare (was work)");
    assert_eq!(host.account(json!({"run": 2, "client": "claude"})), "spare");
    assert!(host.refuses("rota.swap", json!({"to": "old"})).contains("token was refused"));
    assert!(host.refuses("rota.swap", json!({"to": "gone"})).contains("no account 'gone'"));
    // none named: the one with the most room (work and spare are both at 20%, default at 95%)
    let current = host.run("rota.swap", json!({}))["current"].clone();
    assert!(current == "work" || current == "spare", "{current}");

    // renamed (James, Pointman thread 2255 [4136]): shown and typed by the new name, the machine's own
    // login too; its label, and so the runs' account and the health part's id, stay as they were
    let got = host.run("rota.rename", json!({"from": "default", "to": "james-max"}));
    assert_eq!(got["message"], "default is called james-max now");
    let listed = rows(&got);
    assert!(!listed.contains_key("default"));
    assert_eq!(
        (listed["james-max"]["id"].clone(), listed["james-max"]["own_login"].clone()),
        (json!("default"), json!(true))
    );
    host.run("rota.rename", json!({"from": "spare", "to": "team"}));
    assert_eq!(host.run("rota.swap", json!({"to": "team"}))["current"], "team");
    assert_eq!(host.account(json!({"run": 3, "client": "claude"})), "spare");
    assert_eq!(host.parts(|_| true)["spare"]["name"], "Claude: team");
    assert_eq!(host.run("rota.swap", json!({"to": "spare"}))["current"], "team"); // its label still works
    assert!(host
        .refuses("rota.rename", json!({"from": "work", "to": "team"}))
        .contains("another account here is called team"));
    assert!(host
        .refuses("rota.rename", json!({"from": "work", "to": "spare"}))
        .contains("another account here is called spare"));
    assert!(host.refuses("rota.rename", json!({"from": "gone", "to": "x"})).contains("no account 'gone'"));
    assert!(host.refuses("rota.rename", json!({"from": "work", "to": "Not A Name"})).contains("to:"));
    // `/rota add` posted (an agent, an older app) says where accounts are added: never with a token in a post
    let got = host.run("rota.add", json!({"name": "Night"}));
    let said = got["message"].as_str().unwrap_or_default();
    assert!(said.contains("`/rota add night` there and tap Sign in") && said.contains("Or on studio:"), "{said}");
    assert!(host.run("rota.add", json!({"name": "team"}))["message"].as_str().unwrap().starts_with("team is here"));
    assert!(host.refuses("rota.add", json!({"name": "no good"})).contains("name:"));
    // back to its label: the name goes
    host.run("rota.rename", json!({"from": "team", "to": "spare"}));
    host.run("rota.rename", json!({"from": "james-max", "to": "default"}));
    let listed = rows(&host.status());
    assert!(listed.contains_key("spare") && listed.contains_key("default") && !listed.contains_key("team"));

    // a folder's runs start on its own account, whatever the rule picks for the rest
    let agent = host.path("agent");
    fs::create_dir(&agent).unwrap();
    host.run("rota.folder", json!({"folder": agent, "account": "spare"}));
    host.run("rota.swap", json!({"to": "work"}));
    assert_eq!(
        host.call("session.prepare", json!({"run": 4, "client": "claude", "folder": agent})),
        json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": spare()}, "account": "spare"})
    );
    assert_eq!(host.account(json!({"run": 5, "client": "claude", "folder": "/elsewhere"})), "work");
    // ...until it's full: then the rule again
    host.ended("spare", "done", Value::Null, over());
    assert_eq!(host.account(json!({"run": 6, "client": "claude", "folder": agent})), "work");
    assert_eq!(host.run("rota.folder", json!({"folder": agent}))["folders"], json!({}));
}

/// A Codex session as Codex writes it, with the account's windows in its rate_limits.
fn rollout(home: &Path, used: f64, window: u64) {
    let day = home.join("sessions/2026/10/05");
    fs::create_dir_all(&day).unwrap();
    let line = json!({"timestamp": "2026-10-05T12:00:00Z", "type": "event_msg", "payload": {"type": "token_count",
        "rate_limits": {"limit_id": "codex", "primary": {"used_percent": used, "window_minutes": window,
                                                         "resets_at": 4102444800u64}, "secondary": null}}});
    let ns = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    fs::write(day.join(format!("rollout-{ns}.jsonl")), format!("{{\"type\": \"session_meta\"}}\n{line}\n")).unwrap();
}

/// MB-45: a Codex account is a Codex home of its own; a run on it gets CODEX_HOME, and its windows come from
/// the newest session Codex wrote there.
#[test]
fn codex_accounts() {
    let host = Host::start();
    let both = json!({"primary": {"used_percent": 12.5, "window_minutes": 300, "resets_at": 1},
                      "secondary": {"used_percent": 3, "window_minutes": 10080, "resets_at": 2}});
    assert_eq!(
        serde_json::to_value(codex::windows(Some(&both))).unwrap(),
        json!({"five_hour": {"used": 12.5, "resets": 1}, "seven_day": {"used": 3.0, "resets": 2}})
    );
    let (own, spare) = (host.path("codex-own"), host.path("codex-homes/spare"));
    fs::create_dir(&own).unwrap();
    fs::write(own.join("auth.json"), "{}").unwrap();
    // the machine's own login only, so far: runs start on it, with nothing set
    assert_eq!(
        host.call("session.prepare", json!({"run": 1, "client": "codex"})),
        json!({"env": {}, "account": "default"})
    );
    rollout(&own, 95.0, 10080);
    host.call(
        "session.ended",
        json!({"run": 1, "client": "codex", "account": "default", "status": "done", "error": null,
                                      "limits": null}),
    );
    // another account, signed in in a home of its own (as `rota codex add` leaves it)
    fs::create_dir_all(&spare).unwrap();
    fs::write(spare.join("auth.json"), "{}").unwrap();
    let file = host.path("rota.json");
    let mut store = accounts::load_store(Some(&file));
    store.codex = vec![Account::new("spare")];
    accounts::save_store(&store, Some(&file)).unwrap();
    rollout(&spare, 10.0, 10080);
    host.call(
        "session.ended",
        json!({"run": 2, "client": "codex", "account": "spare", "status": "done", "error": null,
                                      "limits": null}),
    );
    assert_eq!(
        host.call("session.prepare", json!({"run": 3, "client": "codex"})),
        json!({"env": {"CODEX_HOME": spare}, "account": "spare"})
    );
    let got = host.parts(|g| g.contains_key("codex.spare"));
    assert!(why(&got, "codex.default").starts_with("week 95%"), "{got:?}");
    assert!(why(&got, "codex.spare").ends_with("runs start here"));
    // Claude's accounts are their own: a Codex move doesn't move Claude runs
    assert_ne!(host.account(json!({"run": 4, "client": "claude"})), "spare");
    assert_eq!(host.status()["codex"]["current"], "spare");
}

/// MB-42: each move is kept with why, for the machine's status; an account runs moved off sits out a
/// cooldown even if it looks fine again; an agent can be mapped to an account (cswap's map).
#[test]
fn moves_cooldown_and_agents() {
    let host = Host::start();
    host.parts(claude_checked);
    assert_eq!(host.account(json!({"run": 1, "handle": "scene", "client": "claude"})), "work");
    host.ended("work", "done", Value::Null, five_hour(0.97));
    assert_eq!(host.account(json!({"run": 2, "handle": "scene", "client": "claude"})), "spare");
    let status = host.status();
    let last = status["moves"].as_array().unwrap().last().unwrap().clone();
    assert_eq!((last["from"].clone(), last["to"].clone()), (json!("work"), json!("spare")));
    assert_eq!(last["why"], "work was at 97%");
    let got = host.parts(|g| why(g, "spare").contains("since"));
    let line = why(&got, "spare");
    assert!(line.contains("runs start here since ") && line.ends_with("(work was at 97%)"), "{line}");

    // spare fills too, and work looks empty again (its window reset): work sits out its cooldown
    host.ended("spare", "done", Value::Null, five_hour(0.95));
    host.ended("work", "done", Value::Null, five_hour(0.10));
    assert_eq!(host.account(json!({"run": 4, "handle": "rig", "client": "claude"})), "spare");

    // spare runs out altogether: runs leave it at once, cooldown or not, for one that can take them
    // (default, at 95%: work is still cooling down)
    host.ended("spare", "limited", json!("usage limit reached"), over());
    assert_eq!(host.account(json!({"run": 6, "handle": "rig", "client": "claude"})), "default");
    host.run("rota.swap", json!({"to": "spare"})); // (back, for what follows)

    // an agent mapped to an account starts there while it can take runs
    host.run("rota.agent", json!({"agent": "@scene", "account": "work"}));
    assert_eq!(
        host.call("session.prepare", json!({"run": 7, "handle": "scene", "client": "claude"})),
        json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": work()}, "account": "work"})
    );

    // a run granted claude.ai connectors starts on this machine's own login (a setup token loads none),
    // with no token in its environment, while that login can take runs; an agent mapped elsewhere still goes
    // there, and with the own login full the run goes where any run would
    let drive = json!(["claude.ai Google Drive"]);
    assert_eq!(
        host.call("session.prepare", json!({"run": 8, "handle": "search", "client": "claude", "connectors": drive})),
        json!({"env": {}, "account": "default"})
    );
    assert_eq!(host.account(json!({"run": 9, "handle": "scene", "client": "claude", "connectors": drive})), "work");
    host.ended("default", "limited", json!("usage limit reached"), over());
    let anywhere = host.account(json!({"run": 10, "handle": "search", "client": "claude", "connectors": []}));
    assert_ne!(anywhere, "default");
    assert_eq!(host.account(json!({"run": 11, "handle": "search", "client": "claude", "connectors": drive})), anywhere);
    assert_eq!(host.run("rota.agent", json!({"agent": "scene"}))["agents"], json!({}));
}

/// MB-216: rota runs Claude Code's own setup-token, gives the node the link that works on any device (never
/// this machine's localhost, and no browser opens here), and turns the pasted code into the token as the
/// payload the vault takes.
#[test]
fn signing_an_account_in_from_the_apps() {
    let host = Host::start();
    let long = Duration::from_secs(60);
    let got = host
        .node
        .call_timeout(
            "credentials.sign_in",
            json!({"form": "account", "label": "work", "person": "james", "session": "sgn_1"}),
            long,
        )
        .unwrap();
    let url = got["url"].as_str().unwrap();
    assert!(url.starts_with("https://claude.com/cai/oauth/authorize") && url.contains("platform.claude.com"), "{url}");
    assert!(!url.contains("localhost") && got["prompt"].as_str().unwrap().contains("paste the code"));
    let e = host.fails("credentials.code", json!({"form": "account", "session": "sgn_1", "code": "wrong-code-0000"}));
    assert!(e.contains("didn't take that code"), "{e}");
    // one try per sign-in
    let e = host.fails("credentials.code", json!({"form": "account", "session": "sgn_1", "code": "good-code-1234"}));
    assert!(e.contains("has ended"), "{e}");
    host.node
        .call_timeout("credentials.sign_in", json!({"form": "account", "label": "work", "session": "sgn_2"}), long)
        .unwrap();
    let got = host
        .node
        .call_timeout(
            "credentials.code",
            json!({"form": "account", "session": "sgn_2", "code": "good-code-1234"}),
            long,
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(got["value"].as_str().unwrap()).unwrap(),
        json!({"kind": "env", "vars": {"CLAUDE_CODE_OAUTH_TOKEN": "sk-ant-oat01-signedinsignedinsignedinsignedin1234"}})
    );
    let e = host.fails("credentials.sign_in", json!({"form": "account", "label": "Not A Label", "session": "sgn_3"}));
    assert!(e.contains("label"), "{e}");
    let e = host.fails("credentials.code", json!({"form": "account", "session": "sgn_3", "code": "x; rm -rf /"}));
    assert!(e.contains("sign-in code"), "{e}");
}

/// A machine moving from the Python rota (0.6.1) keeps its state: the same rota.json, read and written.
#[test]
fn the_state_of_a_python_rota_carries_over() {
    // (checked "later", so the start's checks leave work as it is)
    let python = json!({
        "seen": {"work": {"five_hour": {"used": 46.0, "resets": 4102444800u64},
                          "seven_day": {"used": 37.0, "resets": 4102444800u64},
                          "status": "allowed", "needs_auth": false, "checked_at": 4102444000.5, "error": null}},
        "current": "work", "folders": {"/elsewhere": "spare"}, "agents": {"scene": "work"},
        "left": {"default": 1791298000.25},
        "moves": [{"at": 1791298000.25, "from": "default", "to": "work", "why": "default was at 95%"}],
        "codex": {"seen": {}, "current": null}});
    // the node gives each extension its own folder in its state folder
    let file = |state: &Path| state.join("extensions/rota/rota.json");
    let host = Host::with_state(|state| {
        fs::create_dir_all(file(state).parent().unwrap()).unwrap();
        fs::write(file(state), python.to_string()).unwrap();
    });
    let status = host.status();
    assert_eq!(status["current"], "work");
    assert_eq!(status["agents"], json!({"scene": "work"}));
    assert_eq!(status["folders"], json!({"/elsewhere": "spare"}));
    assert_eq!(status["moves"], python["moves"]);
    assert_eq!(rows(&status)["work"]["five_hour"], json!({"used": 46.0, "resets": 4102444800u64}));
    // and what it writes back has the same shape, so the Python rota could read it again
    host.run("rota.agent", json!({"agent": "rig", "account": "work"}));
    let written: Value = serde_json::from_str(&fs::read_to_string(file(&host.path("state"))).unwrap()).unwrap();
    let keys: Vec<&String> = written.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["agents", "codex", "current", "folders", "left", "moves", "seen"]);
    assert_eq!(written["agents"], json!({"rig": "work", "scene": "work"}));
    let mut kept = python["seen"]["work"].clone();
    kept.as_object_mut().unwrap().remove("error"); // (no error isn't written)
    assert_eq!(written["seen"]["work"], kept);
    assert_eq!(written["moves"], python["moves"]);
    assert_eq!(written["left"], python["left"]);
}
