//! Checking an account nothing has run on lately: a one-line Haiku run through Claude Code itself, whose
//! stream reports the account's limits like any run's (`rate_limit_event`). Claude Code does the talking,
//! on the account's own token, so rota never calls Anthropic's API directly.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::util;

const AUTH: [&str; 6] = ["oauth token", "401", "authentication", "invalid api key", "not logged in", "login"];
/// What a run's environment loses, so it signs in only as the account it's given.
pub const UNSET: [&str; 3] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"];

pub fn find_claude() -> Option<PathBuf> {
    let home = util::expand("~");
    [
        util::which("claude"),
        Some("/opt/homebrew/bin/claude".into()),
        Some(home.join(".local/bin/claude")),
        Some("/usr/local/bin/claude".into()),
        Some(home.join(".claude/local/claude")),
    ]
    .into_iter()
    .flatten()
    .find(|c| c.exists())
}

/// Whether an error says the account's token was refused (it needs signing in again).
pub fn refused(text: Option<&str>) -> bool {
    let low = text.unwrap_or("").to_lowercase();
    AUTH.iter().any(|word| low.contains(word))
}

/// The account's last rate_limit_info, or why there's none.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Checked {
    pub limits: Option<Value>,
    pub error: Option<String>,
    pub refused: bool,
}

/// What a program wrote, and how it ended.
pub struct Ran {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Run a program to its end, or kill it after `timeout` and answer None, as `subprocess.run(…, timeout=…)`.
pub fn run(cmd: &mut Command, timeout: Duration) -> std::io::Result<Option<Ran>> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let drain = |mut from: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut buf = vec![];
            let _ = from.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(Box::new(child.stdout.take().expect("piped")));
    let err = drain(Box::new(child.stderr.take().expect("piped")));
    let end = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    };
    Ok(Some(Ran { status, stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default() }))
}

fn first(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// {limits, error, refused}: the account's last rate_limit_info, or why there's none.
pub fn check(token: Option<&str>, claude: Option<&Path>, timeout: Duration) -> std::io::Result<Checked> {
    let Some(claude) = claude.map(Path::to_path_buf).or_else(find_claude) else {
        return Ok(Checked { error: Some("Claude Code isn't installed here".into()), ..Default::default() });
    };
    // its own empty folder, no MCP servers, no settings and nothing kept: nothing but the one turn
    let folder = tempfile::Builder::new().prefix("rota-check-").tempdir()?;
    let mut cmd = Command::new(&claude);
    cmd.args(["-p", "Reply with OK.", "--model", "haiku", "--output-format", "stream-json", "--verbose"])
        .args(["--strict-mcp-config", "--mcp-config", r#"{"mcpServers":{}}"#, "--setting-sources", ""])
        .args(["--no-session-persistence", "--max-turns", "1"])
        .current_dir(folder.path());
    for name in UNSET {
        cmd.env_remove(name);
    }
    if let Some(token) = token {
        cmd.env("CLAUDE_CODE_OAUTH_TOKEN", token);
    }
    let Some(r) = run(&mut cmd, timeout)? else {
        return Ok(Checked {
            error: Some(format!("no answer in {:.0} s", timeout.as_secs_f64())),
            ..Default::default()
        });
    };
    let (mut limits, mut error) = (None, None);
    for line in r.stdout.lines() {
        let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
        match d.get("type").and_then(Value::as_str) {
            Some("rate_limit_event") if d.get("rate_limit_info").is_some_and(Value::is_object) => {
                limits = d.get("rate_limit_info").cloned();
            }
            Some("result") if util::truthy(d.get("is_error")) => {
                let said = d.get("result").filter(|r| util::truthy(Some(r)));
                error =
                    Some(first(&said.map_or("failed".into(), |r| r.as_str().map_or(r.to_string(), String::from)), 300));
            }
            _ => {}
        }
    }
    if limits.is_none() && error.is_none() && !r.status.success() {
        let code = r.status.code().unwrap_or_else(|| -r.status.signal().unwrap_or(0));
        let last = r.stderr.trim().lines().last().map_or(format!("exited with {code}"), String::from);
        error = Some(first(&last, 300));
    }
    let refused = refused(error.as_deref()) && limits.is_none();
    Ok(Checked { limits, error, refused })
}
