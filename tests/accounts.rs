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
    assert!(cli(&["hold", "work"], &store, "").status.success());
    assert_eq!(tokens(&store), vec![("default".into(), None)]);
    cli(&["use", "work"], &store, "");
    assert_eq!(tokens(&store), vec![("default".into(), None), ("work".into(), Some(work()))]);
    assert_eq!(cli(&["add", "default"], &store, &format!("{}\n", work())).status.code(), Some(2));
    cli(&["remove", "work"], &store, "");
    assert_eq!(accounts::load_store(Some(&store)).accounts, vec![]);
    // an account kept in the vault is held here by its label alone
    assert!(cli(&["hold", "spare"], &store, "").status.success());
    assert_eq!(stored(&store), json!([{"label": "spare", "held": true}]));
}
