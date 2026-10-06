//! Signing a Claude account in from the apps (MB-216; James, thread 2850 [3021]): rota runs Claude Code's
//! own `claude setup-token`, hands the node its sign-in link, and, once the person has signed in on any
//! device and pasted back the code the page showed, hands the node the token for the vault.
//!
//! - **The link:** setup-token prints two. One sends the browser back to this machine's localhost, so it
//!   only works here; the other sends it to platform.claude.com, which shows a code to paste back. That's
//!   the one given out. The browser setup-token would open on this machine is stopped.
//! - **The token** comes only out of `code`, as the payload `/secrets add` makes, for the node to seal into
//!   the vault. Nothing here writes it anywhere, logs it or keeps it.
//! - **A sign-in waits** 15 minutes for its code, then it's dropped.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use regex::Regex;
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::{accounts, probe};

pub const WAIT: Duration = Duration::from_secs(15 * 60);
static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[@-Z\\-_]").unwrap());
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https://[^\s\x07\x1b\]]+").unwrap());
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9#_.~-]{8,512}$").unwrap());

/// Claude Code's setup-token, running in a terminal of its own.
struct Session {
    child: Child,
    pty: OwnedFd,
    _folder: TempDir,
    started: Instant,
    raw: Vec<u8>,
}

impl Session {
    /// Read the program's output until `until` is true (or it ends, or the timeout).
    fn read(&mut self, until: impl Fn(&Session) -> bool, timeout: Duration) -> String {
        let end = Instant::now() + timeout;
        let mut buf = vec![0u8; 65536];
        while Instant::now() < end {
            if until(self) {
                break;
            }
            let mut poll = libc::pollfd { fd: self.pty.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd, and a buffer as long as the length given
            let ready = unsafe { libc::poll(&mut poll, 1, 250) };
            if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
            if ready <= 0 {
                continue;
            }
            let n = unsafe { libc::read(self.pty.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            self.raw.extend_from_slice(&buf[..n as usize]);
        }
        self.text()
    }

    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }

    fn text(&self) -> String {
        ANSI.replace_all(&self.raw_text(), "").into_owned()
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut at = 0;
        while at < bytes.len() {
            let n = unsafe { libc::write(self.pty.as_raw_fd(), bytes[at..].as_ptr().cast(), bytes.len() - at) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            at += n as usize;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The sign-in link that works on any device: the one whose callback isn't this machine's localhost. (A
/// link can come as a terminal hyperlink, so it's read from the raw output too.)
pub fn manual_link(raw: &str) -> Option<String> {
    LINK.find_iter(raw)
        .map(|m| m.as_str())
        .find(|url| url.contains("redirect_uri=") && !url.contains("localhost") && !url.contains("127.0.0.1"))
        .map(String::from)
}

fn failed(text: &str) -> bool {
    let low = text.to_lowercase();
    ["invalid", "error", "failed", "expired"].iter().any(|w| low.contains(w))
}

fn cloexec(fd: libc::c_int) {
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
}

/// A pseudo-terminal, 4000 columns wide so the link and the token aren't wrapped onto several lines:
/// (the side rota reads, the side the program gets).
fn pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut ours, mut theirs) = (0, 0);
    let mut size = libc::winsize { ws_row: 50, ws_col: 4000, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty fills two fds, which are owned from here on
    if unsafe { libc::openpty(&mut ours, &mut theirs, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    cloexec(ours);
    cloexec(theirs);
    Ok(unsafe { (OwnedFd::from_raw_fd(ours), OwnedFd::from_raw_fd(theirs)) })
}

#[derive(Default)]
pub struct SignIns {
    claude: Option<PathBuf>,
    open: Mutex<HashMap<String, Session>>,
}

impl SignIns {
    pub fn new(claude: Option<PathBuf>) -> Self {
        SignIns { claude, open: Mutex::default() }
    }

    /// Start setup-token; answer its sign-in link.
    pub fn start(&self, session_id: &str, timeout: Duration) -> anyhow::Result<Value> {
        let Some(claude) = self.claude.clone().or_else(probe::find_claude) else {
            bail!("Claude Code isn't installed on this machine");
        };
        self.sweep();
        let folder = tempfile::Builder::new().prefix("rota-signin-").tempdir()?;
        // a stand-in for `open` and `xdg-open`: the sign-in happens wherever the person is, not here
        for name in ["open", "xdg-open"] {
            let stub = folder.path().join(name);
            fs::write(&stub, "#!/bin/sh\nexit 0\n")?;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o700))?;
        }
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        let (ours, theirs) = pty()?;
        let mut cmd = Command::new(&claude);
        cmd.arg("setup-token")
            .current_dir(folder.path())
            .env("PATH", format!("{}:{path}", folder.path().display()))
            .env("BROWSER", folder.path().join("open"))
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .stdin(Stdio::from(theirs.try_clone()?))
            .stdout(Stdio::from(theirs.try_clone()?))
            .stderr(Stdio::from(theirs));
        for name in probe::UNSET {
            cmd.env_remove(name);
        }
        // SAFETY: only async-signal-safe calls between fork and exec: its own session, with the
        // terminal as its controlling one
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(cmd); // the program's side of the terminal closes here, so its end reads as the end
        let mut s = Session { child, pty: ours, _folder: folder, started: Instant::now(), raw: vec![] };
        s.read(|s| manual_link(&s.raw_text()).is_some(), timeout);
        let Some(link) = manual_link(&s.raw_text()) else {
            bail!("Claude Code didn't give a sign-in link");
        };
        let old = self.open.lock().unwrap().insert(session_id.to_string(), s);
        drop(old);
        Ok(json!({"url": link, "prompt": "Sign in as the account to add, then paste the code the page shows."}))
    }

    /// Give setup-token the code; answer the token as the vault payload. The session ends either way.
    pub fn code(&self, session_id: &str, code: &str, timeout: Duration) -> anyhow::Result<Value> {
        let code = code.trim();
        if !CODE.is_match(code) {
            bail!("that isn't a sign-in code: paste the code the page showed");
        }
        let mut s = self
            .open
            .lock()
            .unwrap()
            .remove(session_id)
            .ok_or_else(|| anyhow!("this sign-in has ended: start it again"))?;
        let before = s.text().len();
        s.write(format!("{code}\r").as_bytes())?;
        let text = s.read(
            |s| {
                let t = s.text();
                accounts::TOKEN.is_match(&t) || failed(t.get(before..).unwrap_or(""))
            },
            timeout,
        );
        let Some(token) = accounts::find_token(&text) else {
            bail!(if failed(&text) {
                "Claude didn't take that code: start the sign-in again"
            } else {
                "Claude Code didn't give a token"
            });
        };
        let value = json!({"kind": "env", "vars": {"CLAUDE_CODE_OAUTH_TOKEN": token}});
        Ok(json!({"value": value.to_string()}))
    }

    /// Drop sign-ins nobody finished.
    pub fn sweep(&self) {
        let stale: Vec<Session> = {
            let mut open = self.open.lock().unwrap();
            let gone: Vec<String> =
                open.iter().filter(|(_, s)| s.started.elapsed() > WAIT).map(|(k, _)| k.clone()).collect();
            gone.iter().filter_map(|k| open.remove(k)).collect()
        };
        drop(stale);
    }
}
