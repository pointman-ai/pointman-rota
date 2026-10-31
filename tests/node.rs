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
# `claude auth status`: the machine's own login, none while the file FAKE_NO_LOGIN names is there
if [ "$1" = "auth" ]; then
  if [ -e "$FAKE_NO_LOGIN" ]; then echo '{"loggedIn": false, "authMethod": "none"}'; exit 1; fi
  echo '{"loggedIn": true, "authMethod": "claude.ai"}'; exit 0
fi
if [ -z "$CLAUDE_CODE_OAUTH_TOKEN" ] && [ -e "$FAKE_NO_LOGIN" ]; then
  echo '{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}'; exit 1
fi
# its usage (`get_usage`), as on 8 Oct: the machine's own login with credits off, work's 40% of its monthly
# limit, and none for spare (a setup token's, maybe)
input=$(cat)
case "$input" in *get_usage*)
  case "$CLAUDE_CODE_OAUTH_TOKEN" in
    "") echo '{"type":"control_response","response":{"subtype":"success","request_id":"usage","response":{"rate_limits_available":true,"rate_limits":{"extra_usage":{"is_enabled":false,"monthly_limit":null,"used_credits":null,"utilization":null,"currency":null,"spend_limit_reached":false}}}}}' ;;
    *www*) echo '{"type":"control_response","response":{"subtype":"success","request_id":"usage","response":{"rate_limits_available":true,"rate_limits":{"extra_usage":{"is_enabled":true,"monthly_limit":20000,"used_credits":8050,"utilization":40,"currency":"usd","spend_limit_reached":false}}}}}' ;;
    *) echo '{"type":"control_response","response":{"subtype":"error","request_id":"usage","error":"no usage for this token"}}' ;;
  esac ;;
esac
case "$input" in *'"type":"user"'*) ;; *) exit 0 ;; esac
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
        fs::write(bin.join("codex"), FAKE_CODEX).unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
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
            .env("FAKE_NO_LOGIN", at("no-login"))
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
            json!({"run": {"id": "run_1"}, "account": account, "status": format!("STATUS_{}", status.to_uppercase()),
                   "error": error, "limits": limits}),
        );
    }

    /// The health parts by id, once `until` holds (or 30 s on).
    fn parts(&self, until: impl Fn(&BTreeMap<String, Value>) -> bool) -> BTreeMap<String, Value> {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let got: BTreeMap<String, Value> = self.call("health.check", json!({}))["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
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
    got.iter().filter(|(id, _)| !id.starts_with("codex.")).all(|(_, p)| p.get("state").is_some())
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
    assert!(why(&got, "work").starts_with("5h 20%") && got["work"]["state"] == "STATE_OK");
    assert_eq!(got["old"]["needs"], json!(["ACTION_AUTH"]));
    assert_eq!(got["old"]["state"], "STATE_FAILING");
    // an account kept in the vault, as rota.spare, is one too; a name that isn't a label isn't
    assert!(why(&got, "spare").starts_with("5h 20%") && !format!("{got:?}").contains("Not A Label"));
    // the same check asked Claude Code for each one's usage credits: % of the monthly spend limit, in dollars
    let first = rows(&host.status());
    assert_eq!(
        (&first["work"]["credits"]["used"], &first["work"]["credits"]["limit"], &first["work"]["credits"]["percent"]),
        (&json!(80.5), &json!(200), &json!(40)) // (whole numbers come back as integers)
    );
    assert!(first["work"]["line"].as_str().unwrap().ends_with(" · monthly spend 40% ($80.50 of $200)"), "{first:?}");
    assert!(first["default"]["line"].as_str().unwrap().contains(" · usage credits off"));
    assert!(first["spare"].get("credits").is_none());
    // the machine's own login is past 90%, so runs start on work, with its token
    let run =
        host.call("session.prepare", json!({"run": {"id": "run_1"}, "agent": {"name": "scene"}, "client": "claude"}));
    assert_eq!(run, json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": work()}, "account": "work"}));
    assert_eq!(
        host.call("session.prepare", json!({"run": {"id": "run_2"}, "agent": {"name": "scene"}, "client": "gemini"})),
        json!({})
    );
    // a run's own report moves it on: work fills up, so runs go to the vault's spare, with its token
    host.ended("work", "done", Value::Null, five_hour(0.97));
    assert_eq!(
        host.call("session.prepare", json!({"run": {"id": "run_3"}, "client": "claude"})),
        json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": spare()}, "account": "spare"})
    );
    // and spare full too: nothing else has room, so they stay put
    let mut spent = five_hour(0.95);
    spent["overageStatus"] = json!("rejected");
    spent["overageDisabledReason"] = json!("org_level_disabled_until");
    host.ended("spare", "done", Value::Null, spent);
    assert_eq!(host.account(json!({"run": {"id": "run_4"}, "client": "claude"})), "spare");
    // its spend limit stays known through a run whose last event doesn't say
    host.ended("spare", "done", Value::Null, five_hour(0.95));
    let status = host.status();
    assert_eq!(rows(&status)["spare"]["extra"], "spent");
    assert!(rows(&status)["spare"]["line"].as_str().unwrap().contains("monthly spend limit reached"));
    assert_eq!(status["current"], "spare");
    assert!(!status.to_string().contains(&work()));
    let order: Vec<&str> =
        status["accounts"].as_array().unwrap().iter().map(|a| a["label"].as_str().unwrap()).collect();
    assert_eq!(order, ["default", "work", "old", "spare"]);
    assert!(!status.to_string().contains(&spare()));
    let fixed = host.call("health.fix", json!({"part": "old", "action": "ACTION_AUTH"}));
    let message = fixed["message"].as_str().unwrap();
    assert!(message.contains("claude setup-token") && message.contains("rota add old"), "{message}");
}

/// A machine whose Claude Code isn't signed in of its own, a Mac signed in through rota only (EXT-44): its own
/// login isn't failing and asks for nothing, runs go on the added accounts, and once Claude Code is signed in
/// it's an account like the others again.
#[test]
fn a_machine_with_no_claude_login_of_its_own() {
    let host = Host::with_state(|state| fs::write(state.parent().unwrap().join("no-login"), "").unwrap());
    let added_checked = |got: &BTreeMap<String, Value>| {
        why(got, "default").starts_with("no Claude login")
            && got
                .iter()
                .filter(|(id, _)| *id != "default" && !id.starts_with("codex."))
                .all(|(_, p)| p.get("state").is_some())
    };
    let got = host.parts(added_checked);
    assert_eq!(
        why(&got, "default"),
        "no Claude login of its own on this machine: runs use the added accounts",
        "{got:?}"
    );
    assert_eq!(
        (&got["default"]["state"], &got["default"]["needs"]),
        (&Value::Null, &Value::Null),
        "unknown, and needs nothing"
    );
    // an added account whose token is refused still asks to be signed in again
    assert_eq!((&got["old"]["state"], &got["old"]["needs"]), (&json!("STATE_FAILING"), &json!(["ACTION_AUTH"])));
    assert_eq!(host.account(json!({"run": {"id": "run_1"}, "client": "claude"})), "work");
    // nor does rota's event (the node's Needs-you records read it) say its token was refused
    let row = &rows(&host.status())["default"];
    assert_eq!((&row["needs_auth"], &row["no_login"]), (&json!(false), &json!(true)), "{row}");
    let refused = host.refuses("rota.swap", json!({"to": "default"}));
    assert!(refused.contains("no Claude login on studio"), "{refused}");
    // signed in here: a run on it reports its windows, and it's an account like the others
    fs::remove_file(host.path("no-login")).unwrap();
    host.ended("default", "done", Value::Null, five_hour(0.3));
    let got = host.parts(|got| why(got, "default").starts_with("5h 30%"));
    assert!(why(&got, "default").starts_with("5h 30%"), "{got:?}");
    assert_eq!((&got["default"]["state"], &got["default"]["needs"]), (&json!("STATE_OK"), &Value::Null));
    assert!(rows(&host.status())["default"].get("no_login").is_none());
    assert_eq!(host.run("rota.swap", json!({"to": "default"}))["current"], "default");
}

#[test]
fn swapping_renaming_and_folders() {
    let host = Host::start();
    host.parts(claude_checked);
    assert_eq!(host.account(json!({"run": {"id": "run_1"}, "client": "claude"})), "work");
    // asked to, runs move at once, and stay there as after any move
    let got = host.run("rota.swap", json!({"to": "spare"}));
    assert_eq!(got["current"], "spare");
    assert_eq!(got["message"], "New runs on studio start on spare (was work)");
    assert_eq!(host.account(json!({"run": {"id": "run_2"}, "client": "claude"})), "spare");
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
    assert_eq!(host.account(json!({"run": {"id": "run_3"}, "client": "claude"})), "spare");
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
        host.call("session.prepare", json!({"run": {"id": "run_4"}, "client": "claude", "folder": agent})),
        json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": spare()}, "account": "spare"})
    );
    assert_eq!(host.account(json!({"run": {"id": "run_5"}, "client": "claude", "folder": "/elsewhere"})), "work");
    // ...until it's full: then the rule again
    host.ended("spare", "done", Value::Null, over());
    assert_eq!(host.account(json!({"run": {"id": "run_6"}, "client": "claude", "folder": agent})), "work");
    assert_eq!(host.run("rota.folder", json!({"folder": agent}))["folders"], json!({}));
}

#[test]
fn the_accounts_pane_moves_runs_and_keeps_up() {
    let host = Host::start();
    host.parts(claude_checked);
    host.run("rota.swap", json!({"to": "work"}));
    let view = host.node.open_pane("accounts", Value::Null).unwrap();
    // each block as the app gets it: protobuf's standard JSON
    let block = |name: &str| serde_json::to_value(view.block(name).unwrap()).unwrap();
    let subtitle = |name: &str| block(name)["text"]["heading"]["subtitle"].clone();
    assert_eq!(view.title().as_deref(), Some("Rota")); // Claude's accounts, then Codex's (James, thread 8028 [8035])
    assert_eq!(subtitle("head"), "New runs start on work · it swaps at 90%");
    let work = block("account:work");
    let children = &work["layout"]["stack"]["children"];
    assert_eq!(children[0]["text"]["heading"]["subtitle"], "monthly spend 40% ($80.50 of $200) · runs start here");
    assert_eq!(
        (children[1]["chart"]["title"].as_str(), children[1]["chart"]["data"]["rows"][0][0].as_f64()),
        (Some("5-hour"), Some(20.0))
    );
    let buttons = block("actions")["input"]["actions"]["buttons"].clone();
    assert!(buttons.as_array().unwrap().iter().any(|b| b["id"] == "use:spare"), "{buttons}");

    // a button moves new runs, and the pane follows in place: edits, never all of it again
    view.input_json(json!({"kind": "tap", "block": "actions", "value": "use:spare"})).unwrap();
    assert_eq!(subtitle("head"), "New runs start on spare · it swaps at 90%");
    assert_eq!(host.account(json!({"run": {"id": "run_1"}, "client": "claude"})), "spare");
    assert!(view.frames().iter().all(|(method, _)| method == "pane.edit"), "{:?}", view.frames());
    view.input_json(json!({"kind": "tap", "block": "actions", "value": "swap"})).unwrap();

    let refused = view.input_json(json!({"kind": "tap", "block": "actions", "value": "use:old"})).unwrap_err();
    assert!(refused.message.contains("token was refused"), "{refused}");
    let refused = view.input_json(json!({"kind": "tap", "block": "head", "value": null})).unwrap_err();
    assert_eq!(refused.message, "only the buttons do something here");
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

/// A Codex that answers its app server's JSON-RPC as the real one does, from `$CODEX_HOME/usage.json`, or
/// says it isn't signed in (tests/accounts.rs has the same).
const FAKE_CODEX: &str = r#"#!/bin/sh
[ "$1" = "app-server" ] || exit 2
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) echo '{"jsonrpc":"2.0","id":1,"result":{"userAgent":"fake"}}' ;;
    *'"id":2'*) if [ -f "$CODEX_HOME/usage.json" ]; then
                  printf '{"jsonrpc":"2.0","id":2,"result":%s}\n' "$(cat "$CODEX_HOME/usage.json")"
                else
                  echo '{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Not logged in to ChatGPT"}}'
                fi ;;
  esac
done
"#;

/// James, Pointman thread 3359 [7694]: `/rota usage` asks Codex for the usage of each signed-in Codex
/// account, as its /status shows it, and health shows it too.
#[test]
fn codex_usage_in_rota_usage() {
    let host = Host::start();
    let own = host.path("codex-own");
    fs::create_dir(&own).unwrap();
    fs::write(own.join("auth.json"), "{}").unwrap();
    let limits = json!({"primary": {"usedPercent": 7, "windowDurationMins": 10080, "resetsAt": 4102444800u64},
                        "planType": "plus", "credits": {"hasCredits": false, "unlimited": false}});
    fs::write(own.join("usage.json"), json!({"ordinaryUsageAllowed": true, "rateLimits": limits}).to_string()).unwrap();
    let got = host.run("rota.usage", json!({}));
    let codex = &got["codex"]["accounts"][0];
    assert_eq!((codex["id"].as_str(), codex["seven_day"]["used"].as_f64()), (Some("codex.default"), Some(7.0)));
    let line = codex["line"].as_str().unwrap();
    assert!(line.starts_with("week 7% (resets ") && line.ends_with(" · Plus plan"), "{line}");
    assert!(got["text"].as_str().unwrap().contains(&format!("**Codex**\n- **default** · {line}")), "{got}");
    let parts = host.parts(|g| g.contains_key("codex.default"));
    assert_eq!(why(&parts, "codex.default"), line);
}

/// EXT-113: Grok's runs take its keys in turns, moving on after a run on one hits a limit (the node lists the keys
/// this machine holds, `key_labels`, and opens the one rota names, `key_label`); ZCode with one key goes as it did,
/// and a client with no key here gets nothing from rota.
#[test]
fn grok_and_zcode_keys() {
    let host = Host::start();
    // as the node lists them: Grok's own key (""), a labelled one, and a name that isn't a label; ZCode's own only
    let offered = |client: &str| if client == "grok" { json!(["", "work", "Not-A-Label"]) } else { json!([""]) };
    let prepare = |client: &str| {
        host.call("session.prepare", json!({"run": {"id": "run_1"}, "client": client, "keyLabels": offered(client)}))
    };
    // before a Grok run, rota hasn't heard of its keys
    assert!(host.run("rota.usage", json!({}))["keys"].get("grok").is_none());
    let ended = |client: &str, account: &str, status: &str, error: &str| {
        host.call(
            "session.ended",
            json!({"run": {"id": "run_1"}, "client": client, "account": account,
                   "status": format!("STATUS_{}", status.to_uppercase()), "error": error}),
        );
    };
    // the client's own key first: no label, so the node opens grok.key as without rota
    assert_eq!(prepare("grok"), json!({"account": "default"}));
    // a failure that isn't a limit changes nothing
    ended("grok", "default", "failed", "the session ended without a result");
    assert_eq!(prepare("grok"), json!({"account": "default"}));
    // a limit: the next run moves to work, and the node opens grok.key.work for it
    ended("grok", "default", "failed", "429 Too Many Requests: rate limit exceeded for this API key");
    assert_eq!(prepare("grok"), json!({"account": "work", "keyLabel": "work"}));
    assert_eq!(prepare("grok"), json!({"account": "work", "keyLabel": "work"}), "it stays there");
    let usage = host.run("rota.usage", json!({}));
    let grok = &usage["keys"]["grok"];
    assert_eq!(grok["current"], "work");
    let lines: Vec<(&str, &str)> = grok["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| (k["label"].as_str().unwrap(), k["line"].as_str().unwrap()))
        .collect();
    assert_eq!(lines.len(), 2, "a name that isn't a label isn't a key: {lines:?}");
    assert!(lines[0].0 == "default" && lines[0].1.starts_with("sat out after a limit · back at "), "{lines:?}");
    assert_eq!(lines[1], ("work", "runs start here"));
    assert!(grok["keys"][0]["said"].as_str().unwrap().contains("rate limit"));
    let text = usage["text"].as_str().unwrap();
    assert!(
        text.contains("**Grok keys** · another key: `/secrets add grok.key.<name>`\n- **default** · sat out"),
        "{text}"
    );
    // work at its limit too: nowhere else to go, so the one back soonest
    ended("grok", "work", "failed", "Usage limit reached for 5 hour");
    assert_eq!(prepare("grok"), json!({"account": "default"}));
    // a run on it that ends well: it's ready again
    ended("grok", "default", "done", "");
    let grok = &host.run("rota.status", json!({}))["keys"]["grok"];
    assert_eq!(grok["keys"][0]["line"], "runs start here");
    // ZCode has its own key only: every run goes there, a limit or not
    assert_eq!(prepare("zcode"), json!({"account": "default"}));
    ended("zcode", "default", "failed", "Insufficient balance or no resource package");
    assert_eq!(prepare("zcode"), json!({"account": "default"}));
    // no keys at all, or a client rota doesn't take: nothing
    let none = host.call("session.prepare", json!({"run": {"id": "run_1"}, "client": "zcode", "keyLabels": []}));
    assert_eq!(none, json!({}));
    assert!(host.run("rota.usage", json!({}))["keys"].get("zcode").is_none(), "its keys are gone: so is its list");
    assert_eq!(prepare("opencode"), json!({}));
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
        host.call("session.prepare", json!({"run": {"id": "run_1"}, "client": "codex"})),
        json!({"account": "default"})
    );
    rollout(&own, 95.0, 10080);
    host.call(
        "session.ended",
        json!({"run": {"id": "run_1"}, "client": "codex", "account": "default", "status": "STATUS_DONE", "error": null,
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
        json!({"run": {"id": "run_2"}, "client": "codex", "account": "spare", "status": "STATUS_DONE", "error": null,
                                      "limits": null}),
    );
    assert_eq!(
        host.call("session.prepare", json!({"run": {"id": "run_3"}, "client": "codex"})),
        json!({"env": {"CODEX_HOME": spare}, "account": "spare"})
    );
    let got = host.parts(|g| g.contains_key("codex.spare"));
    assert!(why(&got, "codex.default").starts_with("week 95%"), "{got:?}");
    assert!(why(&got, "codex.spare").ends_with("runs start here"));
    // Claude's accounts are their own: a Codex move doesn't move Claude runs
    assert_ne!(host.account(json!({"run": {"id": "run_4"}, "client": "claude"})), "spare");
    assert_eq!(host.status()["codex"]["current"], "spare");
}

/// MB-42: each move is kept with why, for the machine's status; an account runs moved off sits out a
/// cooldown even if it looks fine again; an agent can be mapped to an account (cswap's map).
#[test]
fn moves_cooldown_and_agents() {
    let host = Host::start();
    host.parts(claude_checked);
    assert_eq!(host.account(json!({"run": {"id": "run_1"}, "agent": {"name": "scene"}, "client": "claude"})), "work");
    host.ended("work", "done", Value::Null, five_hour(0.97));
    assert_eq!(host.account(json!({"run": {"id": "run_2"}, "agent": {"name": "scene"}, "client": "claude"})), "spare");
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
    assert_eq!(host.account(json!({"run": {"id": "run_4"}, "agent": {"name": "rig"}, "client": "claude"})), "spare");

    // spare runs out altogether: runs leave it at once, cooldown or not, for one that can take them
    // (default, at 95%: work is still cooling down)
    host.ended("spare", "limited", json!("usage limit reached"), over());
    assert_eq!(host.account(json!({"run": {"id": "run_6"}, "agent": {"name": "rig"}, "client": "claude"})), "default");
    host.run("rota.swap", json!({"to": "spare"})); // (back, for what follows)

    // an agent mapped to an account starts there while it can take runs
    host.run("rota.agent", json!({"agent": "@scene", "account": "work"}));
    assert_eq!(
        host.call("session.prepare", json!({"run": {"id": "run_7"}, "agent": {"name": "scene"}, "client": "claude"})),
        json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": work()}, "account": "work"})
    );

    // a run granted claude.ai connectors starts on this machine's own login (a setup token loads none),
    // with no token in its environment, while that login can take runs; an agent mapped elsewhere still goes
    // there, and with the own login full the run goes where any run would
    let drive = json!(["claude.ai Google Drive"]);
    assert_eq!(
        host.call(
            "session.prepare",
            json!({"run": {"id": "run_8"}, "agent": {"name": "search"}, "client": "claude", "connectors": drive})
        ),
        json!({"account": "default"})
    );
    assert_eq!(
        host.account(
            json!({"run": {"id": "run_9"}, "agent": {"name": "scene"}, "client": "claude", "connectors": drive})
        ),
        "work"
    );
    host.ended("default", "limited", json!("usage limit reached"), over());
    let anywhere = host
        .account(json!({"run": {"id": "run_10"}, "agent": {"name": "search"}, "client": "claude", "connectors": []}));
    assert_ne!(anywhere, "default");
    assert_eq!(
        host.account(
            json!({"run": {"id": "run_11"}, "agent": {"name": "search"}, "client": "claude", "connectors": drive})
        ),
        anywhere
    );
    assert_eq!(host.run("rota.agent", json!({"agent": "scene"}))["agents"], json!({}));
}

/// EXT-101 (James, Pointman thread 8028 [23890]): `/rota swap work scene` moves only that agent's runs; the
/// machine's other runs stay where they were, and `/rota agent scene` puts it back on the rule.
#[test]
fn swapping_one_agent() {
    let host = Host::start();
    host.parts(claude_checked);
    host.run("rota.swap", json!({"to": "spare"}));
    let got = host.run("rota.swap", json!({"to": "work", "agent": "@Scene"}));
    assert_eq!(got["current"], "spare");
    assert_eq!(got["agents"], json!({"scene": "work"}));
    assert_eq!(got["message"], "@scene's new runs start on work; other runs on studio stay on spare");
    let prepare = |run: &str, handle: &str| {
        host.account(json!({"run": {"id": run}, "agent": {"name": handle}, "client": "claude"}))
    };
    assert_eq!(prepare("run_1", "scene"), "work");
    assert_eq!(prepare("run_2", "search"), "spare");
    // none named: the one with the most room (work and spare are both at 20%, default at 95%)
    let room = host.run("rota.swap", json!({"agent": "rig"}))["agents"]["rig"].clone();
    assert!(room == "work" || room == "spare", "{room}");
    // the same checks as a swap of the machine
    assert!(host.refuses("rota.swap", json!({"to": "old", "agent": "scene"})).contains("token was refused"));
    assert!(host.refuses("rota.swap", json!({"to": "gone", "agent": "scene"})).contains("no account 'gone'"));
    assert!(host.refuses("rota.swap", json!({"to": "work", "agent": "@"})).contains("agent: its handle"));
    // rota.agent, now a slash verb, checks a named account as swap does: a typo isn't a silent fall back to the rule
    assert!(host.refuses("rota.agent", json!({"agent": "scene", "account": "wrok"})).contains("no account 'wrok'"));
    assert!(host.refuses("rota.agent", json!({"agent": "scene", "account": "old"})).contains("token was refused"));
    assert_eq!(host.run("rota.agent", json!({"agent": "scene"}))["agents"], json!({"rig": room}));
    assert_eq!(prepare("run_3", "scene"), "spare");
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
            json!({"form": "account", "label": "work", "person": {"name": "james"}, "session": {"id": "sgn_1"}}),
            long,
        )
        .unwrap();
    let url = got["url"].as_str().unwrap();
    assert!(url.starts_with("https://claude.com/cai/oauth/authorize") && url.contains("platform.claude.com"), "{url}");
    assert!(!url.contains("localhost") && got["prompt"].as_str().unwrap().contains("paste the code"));
    let e = host
        .fails("credentials.code", json!({"form": "account", "session": {"id": "sgn_1"}, "code": "wrong-code-0000"}));
    assert!(e.contains("didn't take that code"), "{e}");
    // one try per sign-in
    let e = host
        .fails("credentials.code", json!({"form": "account", "session": {"id": "sgn_1"}, "code": "good-code-1234"}));
    assert!(e.contains("has ended"), "{e}");
    host.node
        .call_timeout(
            "credentials.sign_in",
            json!({"form": "account", "label": "work", "session": {"id": "sgn_2"}}),
            long,
        )
        .unwrap();
    let got = host
        .node
        .call_timeout(
            "credentials.code",
            json!({"form": "account", "session": {"id": "sgn_2"}, "code": "good-code-1234"}),
            long,
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(got["value"].as_str().unwrap()).unwrap(),
        json!({"kind": "env", "vars": {"CLAUDE_CODE_OAUTH_TOKEN": "sk-ant-oat01-signedinsignedinsignedinsignedin1234"}})
    );
    let e = host
        .fails("credentials.sign_in", json!({"form": "account", "label": "Not A Label", "session": {"id": "sgn_3"}}));
    assert!(e.contains("label"), "{e}");
    let e =
        host.fails("credentials.code", json!({"form": "account", "session": {"id": "sgn_3"}, "code": "x; rm -rf /"}));
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
    assert_eq!(rows(&status)["work"]["five_hour"], json!({"used": 46, "resets": 4102444800u64}));
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
