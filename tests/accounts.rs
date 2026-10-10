//! Rota's rule that picks an account, and its store of tokens through the command line. The scenarios and
//! assertions are the Python rota's (0.6.1), one for one; tests/node.rs has the extension under a node.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use rota::accounts::{self, Seen, Window};
use serde_json::{json, Value};
use tempfile::TempDir;

const RESETS: f64 = 4102444800.0;

fn limits() -> Value {
    json!({"status": "allowed", "rateLimitType": "five_hour", "resetsAt": 4102444800u64,
           "unifiedWindows": {"five_hour": {"resetsAt": 4102444800u64, "utilization": 0.46},
                              "seven_day": {"resetsAt": 4102444800u64, "utilization": 0.37}}})
}

fn work() -> String {
    format!("sk-ant-oat01-{}", "w".repeat(40))
}

fn seen(five: f64, week: f64) -> Seen {
    Seen {
        five_hour: Some(Window { used: five, resets: Some(RESETS) }),
        seven_day: Some(Window { used: week, resets: Some(RESETS) }),
        ..Default::default()
    }
}

fn refused(s: Seen) -> Seen {
    Seen { needs_auth: Some(true), ..s }
}

fn labels(all: &[&str]) -> Vec<String> {
    all.iter().map(|s| s.to_string()).collect()
}

fn fill(all: &[(&str, Seen)]) -> BTreeMap<String, Seen> {
    all.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

#[test]
fn windows_and_how_full() {
    let w = accounts::windows(Some(&limits()));
    assert_eq!(
        serde_json::to_value(&w).unwrap(),
        json!({"five_hour": {"used": 46.0, "resets": 4102444800u64}, "seven_day": {"used": 37.0, "resets": 4102444800u64},
               "status": "allowed"})
    );
    assert_eq!(accounts::used(Some(&w), None), Some(46.0));
    assert_eq!(accounts::used(Some(&Seen::default()), None), None);
    // a window that has reset counts as empty; over its limit counts as full until it resets
    let reset = Seen {
        five_hour: Some(Window { used: 99.0, resets: Some(1.0) }),
        seven_day: Some(Window { used: 20.0, resets: None }),
        ..Default::default()
    };
    assert_eq!(accounts::used(Some(&reset), None), Some(20.0));
    assert_eq!(accounts::used(Some(&Seen { blocked_until: Some(RESETS), ..w }), None), Some(100.0));
    // a warning is still allowed: this is a real run's, the week past a mark at 52%, `resetsAt` its reset
    let warned = accounts::windows(Some(&json!({
        "status": "allowed_warning", "resetsAt": 4102444800u64, "rateLimitType": "seven_day", "utilization": 0.52,
        "unifiedWindows": {"five_hour": {"utilization": 0.28, "resetsAt": 4102444800u64},
                           "seven_day": {"utilization": 0.52, "resetsAt": 4102444800u64}}})));
    assert_eq!(warned.blocked_until, None);
    assert_eq!(accounts::used(Some(&warned), None), Some(52.0));
    // rejected is over the limit: full until its reset
    let over = accounts::windows(Some(&json!({"status": "rejected", "resetsAt": 4102444800u64,
        "unifiedWindows": {"five_hour": {"utilization": 0.3, "resetsAt": 4102444800u64}}})));
    assert_eq!(accounts::used(Some(&over), None), Some(100.0));
}

#[test]
fn usage_credits() {
    // a real run's last event (aurelio-team, 8 Oct): its 5 hours full and its monthly spend limit reached,
    // which Claude Code words as "You've hit your monthly spend limit"
    let spent = accounts::windows(Some(&json!({
        "status": "rejected", "resetsAt": 4102444800u64, "rateLimitType": "five_hour", "overageStatus": "rejected",
        "overageDisabledReason": "org_level_disabled_until", "isUsingOverage": false,
        "unifiedWindows": {"five_hour": {"utilization": 1, "resetsAt": 4102444800u64},
                           "seven_day": {"utilization": 0.43, "resetsAt": 4102444800u64}}})));
    assert_eq!(spent.extra.as_deref(), Some("spent"));
    let line = accounts::describe("work", Some(&spent), None, Some(RESETS - 3600.0));
    assert!(line.contains("week 43% (resets ") && line.ends_with(" · monthly spend limit reached"), "{line}");
    let said = |info: Value| accounts::windows(Some(&info)).extra;
    assert_eq!(
        said(json!({"overageStatus": "rejected", "overageDisabledReason": "org_level_disabled"})).as_deref(),
        Some("off")
    );
    assert_eq!(
        said(json!({"overageStatus": "rejected", "overageDisabledReason": "out_of_credits"})).as_deref(),
        Some("no_credits")
    );
    assert_eq!(said(json!({"overageStatus": "allowed", "isUsingOverage": false})).as_deref(), Some("on"));
    assert_eq!(said(json!({"overageStatus": "allowed", "isUsingOverage": true})).as_deref(), Some("using"));
    // a warning says nothing of credits: rota keeps what it knew
    assert_eq!(said(json!({"status": "allowed_warning", "isUsingOverage": false})), None);
    assert_eq!(
        said(json!({"overageStatus": "rejected", "overageDisabledReason": "org_spend_cap_reached"})).as_deref(),
        Some("spent")
    );
}

#[test]
fn the_monthly_spend_limit() {
    // Claude Code's `get_usage`: cents, and a %
    let usage = |x: Value| accounts::credits(Some(&json!({"extra_usage": x})), 1.0).unwrap();
    let some = usage(json!({"is_enabled": true, "monthly_limit": 20000, "used_credits": 8050, "utilization": 40.25,
                            "currency": "usd", "spend_limit_reached": false}));
    assert_eq!(
        (some.used, some.limit, some.percent, some.currency.as_deref()),
        (Some(80.5), Some(200.0), Some(40.2), Some("USD"))
    );
    let line = |c: accounts::Credits, extra: Option<&str>| {
        let seen = Seen { credits: Some(c), extra: extra.map(String::from), ..seen(10.0, 20.0) };
        accounts::describe("work", Some(&seen), None, Some(RESETS - 3600.0))
    };
    assert!(line(some.clone(), None).ends_with(" · monthly spend 40% ($80.50 of $200)"));
    // reached, by its own word or a run's since
    let full = usage(json!({"is_enabled": true, "monthly_limit": 20000, "used_credits": 20000, "utilization": 100,
                            "spend_limit_reached": true}));
    assert!(line(full, None).ends_with(" · monthly spend limit reached ($200 of $200)"));
    assert!(line(some, Some("spent")).ends_with(" · monthly spend limit reached ($80.50 of $200)"));
    let unlimited =
        usage(json!({"is_enabled": true, "monthly_limit": null, "used_credits": 1234, "utilization": null}));
    assert!(line(unlimited, None).ends_with(" · usage credits on, $12.34 this month, no limit"));
    let off = usage(json!({"is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null}));
    assert!(line(off, Some("off")).ends_with(" · usage credits off"));
    assert_eq!(accounts::credits(Some(&json!({"five_hour": {}})), 1.0), None);
}

#[test]
fn the_rule() {
    let all = labels(&["default", "work", "spare"]);
    let mut f = fill(&[("default", seen(50.0, 0.0)), ("work", seen(10.0, 0.0)), ("spare", seen(30.0, 0.0))]);
    // runs stay where they are while it has room: a session's cache stays warm
    assert_eq!(accounts::choose(&all, &f, Some("default"), 90.0, None), "default");
    // past the threshold they move to the one with the most room
    f.insert("default".into(), seen(91.0, 0.0));
    assert_eq!(accounts::choose(&all, &f, Some("default"), 90.0, None), "work");
    // not to one that's nearly as full: stay put rather than flip back and forth
    let f = fill(&[("default", seen(92.0, 0.0)), ("work", seen(85.0, 0.0)), ("spare", seen(88.0, 95.0))]);
    assert_eq!(accounts::choose(&all, &f, Some("default"), 90.0, None), "default");
    // one not checked yet beats staying on a full one
    let more = labels(&["default", "work", "spare", "new"]);
    assert_eq!(accounts::choose(&more, &f, Some("default"), 90.0, None), "new");
    // an account whose token was refused is skipped
    let f = fill(&[("default", seen(95.0, 0.0)), ("work", refused(seen(5.0, 0.0))), ("spare", seen(40.0, 0.0))]);
    assert_eq!(accounts::choose(&all, &f, Some("default"), 90.0, None), "spare");
    assert_eq!(accounts::choose(&[], &BTreeMap::new(), None, 90.0, None), "default");
}

#[test]
fn describe() {
    let line = accounts::describe("work", Some(&seen(46.0, 37.0)), Some("work"), Some(RESETS - 3600.0));
    assert!(
        line.starts_with("5h 46% (resets ") && line.contains("week 37%") && line.ends_with("runs start here"),
        "{line}"
    );
    assert_eq!(accounts::describe("spare", None, Some("work"), None), "not checked yet");
}

#[test]
fn the_manifest_and_the_crate_have_one_version() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("extension.toml")).unwrap();
    let version = manifest.lines().find_map(|l| l.strip_prefix("version = ")).unwrap().trim_matches('"');
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

fn cli(args: &[&str], store: &Path, stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rota"))
        .args(args)
        .env("ROTA_STORE", store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn stored(store: &Path) -> Value {
    serde_json::from_str::<Value>(&fs::read_to_string(store).unwrap()).unwrap()["accounts"].clone()
}

fn tokens(store: &Path) -> Vec<(String, Option<String>)> {
    accounts::tokens(&accounts::load_store(Some(store)))
}

#[test]
fn the_cli_keeps_tokens_for_this_user_only() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("rota.json");
    assert_eq!(cli(&["add", "work"], &store, "not a token\n").status.code(), Some(1));
    let r = cli(&["add", "Work"], &store, &format!("{}\n", work()));
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(fs::metadata(&store).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(stored(&store), json!([{"label": "work", "token": work()}]));
    let out = String::from_utf8(cli(&["list"], &store, "").stdout).unwrap();
    assert!(out.contains("work") && !out.contains(&work()));
    assert_eq!(tokens(&store), vec![("default".into(), None), ("work".into(), Some(work()))]);
    assert_eq!(cli(&["add", "default"], &store, &format!("{}\n", work())).status.code(), Some(2));
    cli(&["remove", "work"], &store, "");
    assert_eq!(accounts::load_store(Some(&store)).accounts, vec![]);
    // holding is gone (James, Pointman thread 2255 [4136]): one held before 0.9.0 takes runs again, and
    // its old `held` stays in the file untouched
    fs::write(&store, json!({"accounts": [{"label": "work", "token": work(), "held": true}]}).to_string()).unwrap();
    assert_eq!(tokens(&store), vec![("default".into(), None), ("work".into(), Some(work()))]);
    assert!(!cli(&["hold", "work"], &store, "").status.success());
}

/// The real Claude Code on this machine's own login: a check has its limits and its usage credits. Run by
/// hand: `cargo test --test accounts real_check -- --ignored`.
#[test]
#[ignore]
fn real_check() {
    let got = rota::probe::check(None, None, std::time::Duration::from_secs(120)).unwrap();
    assert!(got.limits.is_some() && got.error.is_none(), "{got:?}");
    let credits = accounts::credits(got.usage.as_ref(), 1.0);
    assert!(credits.is_some(), "{:?}", got.usage_error);
    let alone = rota::probe::usage(None, None, std::time::Duration::from_secs(60)).unwrap();
    assert_eq!((alone.limits, accounts::credits(alone.usage.as_ref(), 1.0)), (None, credits));
}

/// Codex's app server as `account/rateLimits/read` answers (from a real one, 11 Oct 2026; ids cut).
fn codex_usage(used: i64, reached: Option<&str>) -> Value {
    let limits = json!({"limitId": "codex", "planType": "self_serve_business_prolite", "spendControlReached": false,
                        "primary": {"usedPercent": used, "windowDurationMins": 10080, "resetsAt": 4102444800u64},
                        "secondary": {"usedPercent": 3, "windowDurationMins": 300, "resetsAt": 4102400000u64},
                        "credits": {"hasCredits": false, "unlimited": false, "balance": null},
                        "rateLimitReachedType": reached});
    json!({"ordinaryUsageAllowed": reached.is_none(), "rateLimits": limits, "rateLimitsByLimitId": {"codex": limits},
           "rateLimitResetCredits": {"availableCount": 3, "credits": null}, "rateLimitUpsell": null})
}

/// James, Pointman thread 3359 [7694]: a Codex account's usage as Codex's /status shows it.
#[test]
fn codex_usage_as_its_app_server_gives_it() {
    let got = rota::codex::read(&codex_usage(1, None));
    assert_eq!(got.seven_day, Some(Window { used: 1.0, resets: Some(RESETS) }));
    assert_eq!(got.five_hour, Some(Window { used: 3.0, resets: Some(4102400000.0) }));
    assert_eq!((got.status.as_deref(), got.blocked_until), (Some("allowed"), None));
    let line = accounts::describe("default", Some(&got), None, Some(1.0));
    assert!(line.starts_with("5h 3% (resets ") && line.contains(" · week 1% (resets "), "{line}");
    assert!(line.ends_with(" · Business plan · 3 free resets"), "{line}");
    // out of room: blocked until its full window resets, so the rule skips it
    let full = rota::codex::read(&codex_usage(100, Some("rate_limit_reached")));
    assert_eq!((full.status.as_deref(), full.blocked_until), (Some("rate_limit_reached"), Some(RESETS)));
    assert_eq!(accounts::used(Some(&full), Some(1.0)), Some(100.0));
    assert!(accounts::describe("default", Some(&full), None, Some(1.0)).contains(" · limit reached · "));
    // credits, and a plan Codex doesn't know
    let mut rich = codex_usage(5, None);
    rich["rateLimits"]["credits"] = json!({"hasCredits": true, "unlimited": false, "balance": "120"});
    rich["rateLimits"]["planType"] = json!("pro");
    rich["rateLimitsByLimitId"] = Value::Null;
    rich["rateLimitResetCredits"]["availableCount"] = json!(1);
    let line = accounts::describe("x", Some(&rota::codex::read(&rich)), None, Some(1.0));
    assert!(line.ends_with(" · Pro plan · 120 credits · 1 free reset"), "{line}");
}

/// A Codex that answers its app server's JSON-RPC as the real one does, from `$CODEX_HOME/usage.json`, or
/// says it isn't signed in.
const FAKE_CODEX: &str = r#"#!/bin/sh
[ "$1" = "app-server" ] || exit 2
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) echo '{"jsonrpc":"2.0","method":"remoteControl/status/changed","params":{}}'
                echo '{"jsonrpc":"2.0","id":1,"result":{"userAgent":"fake"}}' ;;
    *'"id":2'*) if [ -f "$CODEX_HOME/usage.json" ]; then
                  printf '{"jsonrpc":"2.0","id":2,"result":%s}\n' "$(cat "$CODEX_HOME/usage.json")"
                else
                  echo '{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Not logged in to ChatGPT"}}'
                fi ;;
  esac
done
"#;

#[test]
fn a_codex_check_asks_its_app_server_in_the_accounts_home() {
    let tmp = TempDir::new().unwrap();
    let codex = tmp.path().join("codex");
    fs::write(&codex, FAKE_CODEX).unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let homes = tmp.path().join("homes");
    fs::create_dir_all(homes.join("spare")).unwrap();
    fs::write(homes.join("spare/usage.json"), codex_usage(42, None).to_string()).unwrap();
    std::env::set_var("ROTA_CODEX_HOMES", &homes);
    let got = rota::codex::check("spare", Some(&codex), std::time::Duration::from_secs(10)).unwrap();
    assert_eq!(got.seven_day.map(|w| w.used), Some(42.0));
    let e = rota::codex::check("none", Some(&codex), std::time::Duration::from_secs(10)).unwrap_err().to_string();
    assert_eq!(e, "Codex: Not logged in to ChatGPT");
    assert!(rota::probe::refused(Some(&e)), "an account to sign in again");
}

/// This machine's own Codex login, asked for real: by hand,
/// `cargo test --test accounts real_codex_check -- --ignored --nocapture`.
#[test]
#[ignore]
fn real_codex_check() {
    let got = rota::codex::check(accounts::DEFAULT, None, std::time::Duration::from_secs(30)).unwrap();
    println!("{}", accounts::describe(accounts::DEFAULT, Some(&got), None, None));
    assert!(got.codex.is_some_and(|c| c.plan.is_some()));
}
