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

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use pointman_extension::protocol::CredentialsSignInResponse;
use regex::Regex;
use serde_json::json;
use tempfile::TempDir;

use crate::{accounts, probe};

pub const WAIT: Duration = Duration::from_secs(15 * 60);
/// The terminal's height, as `pty` makes it (cursor moves stop at its top line).
const ROWS: i64 = 50;
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https://[^\s\x07\x1b\]]+").unwrap());
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9#_.~-]{8,512}$").unwrap());

/// Claude Code's setup-token, running in a terminal of its own.
struct Session {
    /// taken when the session ends, to be reaped in the background
    child: Option<Child>,
    /// our end of its terminal: closed first when the session ends
    pty: Option<OwnedFd>,
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
            let mut poll = libc::pollfd { fd: self.fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd, and a buffer as long as the length given
            let ready = unsafe { libc::poll(&mut poll, 1, 250) };
            if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
            if ready <= 0 {
                continue;
            }
            let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            self.raw.extend_from_slice(&buf[..n as usize]);
        }
        self.text()
    }

    fn fd(&self) -> libc::c_int {
        self.pty.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }

    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }

    /// What the terminal shows now (see `screen`).
    fn text(&self) -> String {
        screen(&self.raw_text())
    }

    /// The tokens the screen has shown so far, longest first.
    fn tokens(&self) -> Vec<String> {
        tokens_shown(&self.raw_text())
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut at = 0;
        while at < bytes.len() {
            let n = unsafe { libc::write(self.fd(), bytes[at..].as_ptr().cast(), bytes.len() - at) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            at += n as usize;
        }
        Ok(())
    }
}

impl Drop for Session {
    /// Ends setup-token without waiting on it. Our end of its terminal closes first: on macOS a program
    /// killed with output still unread can't finish closing its terminal, so waiting on it with our end open
    /// waited forever (12 Oct: the token was made, and `code` never answered it). Its exit is reaped in the
    /// background, so nothing that answers the node ever waits on it.
    fn drop(&mut self) {
        let child = self.child.take();
        if let Some(mut c) = child {
            let _ = c.kill();
            drop(self.pty.take());
            std::thread::spawn(move || {
                let _ = c.wait();
            });
        }
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

fn failures(text: &str) -> usize {
    let low = text.to_lowercase();
    ["invalid", "error", "failed", "expired"].iter().map(|w| low.matches(w).count()).sum()
}

/// What a terminal shows for this output. Claude Code redraws only the cells that change and moves the
/// cursor over the rest, so its text can't be read by dropping the escape codes: a letter already on
/// screen isn't written again ("Requ", a move, "st" for "Request"; MB-216, James's sign-in in thread
/// 2850 [3773]). So this is a small terminal: printable characters, CR, LF, BS, tab, and the cursor
/// moves and erases (CSI A–H, J, K) Claude Code uses. Colours, modes and links are skipped.
pub fn screen(raw: &str) -> String {
    render(raw).0
}

/// Every token the screen showed at any point, longest first: Claude Code shows the token, then clears
/// that part of its screen as it finishes, so what's left at the end may not have it (James's second
/// try, thread 2850 [3787]). A line is looked at before anything erases or overwrites it.
pub fn tokens_shown(raw: &str) -> Vec<String> {
    let mut found = render(raw).1;
    found.sort_by_key(|t| std::cmp::Reverse(t.len()));
    found.dedup();
    found
}

fn render(raw: &str) -> (String, Vec<String>) {
    let ch: Vec<char> = raw.chars().collect();
    let mut found: Vec<String> = Vec::new();
    let peek = |row: &Vec<char>, found: &mut Vec<String>| {
        let line: String = row.iter().collect();
        if line.contains("sk-ant-") {
            found.extend(accounts::TOKEN.find_iter(&line).map(|m| m.as_str().to_string()));
        }
    };
    let mut rows: BTreeMap<i64, Vec<char>> = BTreeMap::new();
    let (mut r, mut c, mut bottom) = (0i64, 0usize, 0i64);
    let mut i = 0;
    while i < ch.len() {
        match ch[i] {
            '\x1b' => match ch.get(i + 1) {
                Some('[') => {
                    let mut j = i + 2;
                    while j < ch.len() && !('@'..='~').contains(&ch[j]) {
                        j += 1;
                    }
                    if j >= ch.len() {
                        break;
                    }
                    let params: String = ch[i + 2..j].iter().collect();
                    let private = params.starts_with(['?', '>', '=', '<']);
                    let nums: Vec<i64> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
                    let n = |k: usize| nums.get(k).copied().filter(|v| *v > 0).unwrap_or(1);
                    let top = (bottom - (ROWS - 1)).max(0);
                    if !private {
                        match ch[j] {
                            'A' => r = (r - n(0)).max(top),
                            'B' => r += n(0),
                            'C' => c += n(0) as usize,
                            'D' => c = c.saturating_sub(n(0) as usize),
                            'E' => (r, c) = (r + n(0), 0),
                            'F' => (r, c) = ((r - n(0)).max(top), 0),
                            'G' => c = (n(0) - 1) as usize,
                            'H' | 'f' => (r, c) = (top + n(0) - 1, (n(1) - 1) as usize),
                            'K' => {
                                let row = rows.entry(r).or_default();
                                peek(row, &mut found);
                                match nums.first().copied().unwrap_or(0) {
                                    0 => row.truncate(c),
                                    1 => row.iter_mut().take(c + 1).for_each(|x| *x = ' '),
                                    _ => row.clear(),
                                }
                            }
                            'J' => {
                                rows.values().for_each(|row| peek(row, &mut found));
                                match nums.first().copied().unwrap_or(0) {
                                    0 => {
                                        rows.entry(r).or_default().truncate(c);
                                        rows.retain(|k, _| *k <= r);
                                    }
                                    1 => {
                                        rows.retain(|k, _| *k >= r);
                                        rows.entry(r).or_default().iter_mut().take(c + 1).for_each(|x| *x = ' ');
                                    }
                                    _ => rows.clear(),
                                }
                            }
                            _ => {}
                        }
                    }
                    i = j + 1;
                }
                Some(']') => {
                    let mut j = i + 2;
                    while j < ch.len() && ch[j] != '\x07' && !(ch[j] == '\x1b' && ch.get(j + 1) == Some(&'\\')) {
                        j += 1;
                    }
                    i = if ch.get(j) == Some(&'\x07') { j + 1 } else { j + 2 };
                }
                Some('(' | ')' | '*' | '+') => i += 3,
                Some(_) => i += 2,
                None => break,
            },
            '\r' => {
                c = 0;
                i += 1;
            }
            '\n' => {
                r += 1;
                i += 1;
            }
            '\x08' => {
                c = c.saturating_sub(1);
                i += 1;
            }
            '\t' => {
                c = (c / 8 + 1) * 8;
                i += 1;
            }
            x if (x as u32) < 0x20 || x == '\x7f' => i += 1,
            x => {
                let row = rows.entry(r).or_default();
                if row.len() <= c {
                    row.resize(c + 1, ' ');
                }
                if row[c] != x && row[c] != ' ' {
                    peek(row, &mut found); // a letter written over another: the line as it was
                }
                row[c] = x;
                c += 1;
                i += 1;
            }
        }
        bottom = bottom.max(r);
    }
    rows.values().for_each(|row| peek(row, &mut found));
    let text = rows.values().map(|row| row.iter().collect::<String>().trim_end().to_string()).collect::<Vec<_>>();
    (text.join("\n").trim_end_matches('\n').to_string(), found)
}

/// What Claude Code wrote in a sign-in that failed, with its escape codes (they're the point), and the
/// code and any token taken out.
fn keep_transcript(dir: &Path, raw: &str, code: &str) {
    let redacted = accounts::TOKEN.replace_all(raw, "sk-ant-<token>").replace(code, "<code>");
    let path = dir.join("signin-last.log");
    let wrote = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| io::Write::write_all(&mut f, redacted.as_bytes()));
    if let Err(e) = wrote {
        eprintln!("rota: couldn't keep the sign-in's transcript in {}: {e}", path.display());
    }
}

/// The screen's last few lines, for a sign-in that failed: what Claude Code said, never a token.
fn last_said(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(2)..].join(" / ");
    let tail = Regex::new(r"sk-ant-\S*").unwrap().replace_all(&tail, "sk-ant-…").into_owned();
    tail.chars().take(200).collect()
}

fn cloexec(fd: libc::c_int) {
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
}

/// A pseudo-terminal, 4000 columns wide so the link and the token aren't wrapped onto several lines:
/// (the side rota reads, the side the program gets).
fn pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut ours, mut theirs) = (0, 0);
    let mut size = libc::winsize { ws_row: 50, ws_col: 4000, ws_xpixel: 0, ws_ypixel: 0 };
    // (a raw pointer: macOS's openpty takes *mut winsize, Linux's *const, and *mut passes as either)
    let size: *mut libc::winsize = &mut size;
    // SAFETY: openpty fills two fds, which are owned from here on
    if unsafe { libc::openpty(&mut ours, &mut theirs, std::ptr::null_mut(), std::ptr::null_mut(), size) } != 0 {
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
    pub fn start(&self, session_id: &str, timeout: Duration) -> anyhow::Result<CredentialsSignInResponse> {
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
        let mut s =
            Session { child: Some(child), pty: Some(ours), _folder: folder, started: Instant::now(), raw: vec![] };
        s.read(|s| manual_link(&s.raw_text()).is_some(), timeout);
        let Some(link) = manual_link(&s.raw_text()) else {
            bail!("Claude Code didn't give a sign-in link");
        };
        let old = self.open.lock().unwrap().insert(session_id.to_string(), s);
        drop(old);
        Ok(CredentialsSignInResponse {
            url: link,
            prompt: Some("Sign in as the account to add, then paste the code the page shows.".into()),
            ..Default::default()
        })
    }

    /// Give setup-token the code; answer the token as the vault payload. The session ends either way. One
    /// that fails leaves what Claude Code wrote, codes and tokens taken out, in `keep` (rota's state
    /// folder) as signin-last.log, readable by this user only, so the next fix needn't be a guess.
    pub fn code(&self, session_id: &str, code: &str, timeout: Duration, keep: Option<&Path>) -> anyhow::Result<String> {
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
        let before = failures(&s.text());
        // the code, then Enter on its own: Claude Code takes a long input in one go as a paste, and a
        // pasted Enter doesn't submit (James's 92-character code sat in the field, thread 2850 [3787])
        s.write(code.as_bytes())?;
        s.read(|_| false, Duration::from_millis(500));
        s.write(b"\r")?;
        let text = s.read(|s| !s.tokens().is_empty() || failures(&s.text()) > before, timeout);
        // a moment more, in case the token was still being drawn when it first matched
        let text = if s.tokens().is_empty() { text } else { s.read(|_| false, Duration::from_millis(500)) };
        let Some(token) = s.tokens().into_iter().next() else {
            if let Some(dir) = keep {
                keep_transcript(dir, &s.raw_text(), code);
            }
            bail!(if failures(&text) > before {
                format!("Claude didn't take that code: start the sign-in again ({})", last_said(&text))
            } else {
                format!("Claude Code didn't give a token (it showed: {})", last_said(&text))
            });
        };
        let value = json!({"kind": "env", "vars": {"CLAUDE_CODE_OAUTH_TOKEN": token}});
        Ok(value.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A program killed with its output still unread can't finish closing its terminal on macOS, so ending
    /// a session must never wait on it (12 Oct: a sign-in's token was made and `code` never answered).
    #[test]
    fn ending_a_session_never_waits_on_its_program() {
        let (ours, theirs) = pty().unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "while :; do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done"])
            .stdin(Stdio::from(theirs.try_clone().unwrap()))
            .stdout(Stdio::from(theirs.try_clone().unwrap()))
            .stderr(Stdio::from(theirs));
        // SAFETY: as in `start`: its own session, with the terminal as its controlling one
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        drop(cmd);
        std::thread::sleep(Duration::from_millis(500)); // its output fills the terminal: nobody reads it
        let s = Session {
            child: Some(child),
            pty: Some(ours),
            _folder: tempfile::tempdir().unwrap(),
            started: Instant::now(),
            raw: vec![],
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(s);
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(Duration::from_secs(3)).is_ok(), "ending the session waited on its program");
    }

    #[test]
    fn the_screen_keeps_letters_only_redrawn_where_they_changed() {
        // as Claude Code drew a test code's error: the cursor moved over the 'e' of "Request", already on
        // screen from an earlier frame, so dropping the escape codes would read "Requst"
        let raw = format!(
            "{}e\r\x1b[1COAuth error: Requ\x1b[20Gst\x1b[23Gfailed with\x1b[35Gstatus code 400\x1b[K",
            " ".repeat(18)
        );
        assert_eq!(screen(&raw), " OAuth error: Request failed with status code 400");
        // a token drawn over an earlier one in the same place: only the letters that changed are written
        let first = format!("Token:\r\nsk-ant-oat01-{}\r\n", "A".repeat(40));
        let shown = screen(&(first + "\x1b[1A\x1b[18GBBBB\x1b[40GCC\r\x1b[1B"));
        let want = format!("sk-ant-oat01-{}BBBB{}CC{}", "A".repeat(4), "A".repeat(18), "A".repeat(12));
        assert_eq!(shown, format!("Token:\n{want}"));
        assert_eq!(accounts::find_token(&shown).as_deref(), Some(want.as_str()));
    }

    #[test]
    fn a_token_is_caught_even_when_the_screen_clears_it_after() {
        let token = format!("sk-ant-oat01-{}", "t0K-".repeat(20));
        // shown under the link, then the whole area below the link erased as Claude Code finishes
        let erased = format!("https://claude.com/x\r\nPaste code > \r\nYour token:\r\n{token}\r\n\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K");
        assert_eq!(screen(&erased), "https://claude.com/x");
        assert_eq!(tokens_shown(&erased), vec![token.clone()]);
        // or drawn over by the next frame without an erase
        let over = format!("{token}\r{}", "x".repeat(token.len()));
        assert_eq!(tokens_shown(&over), vec![token.clone()]);
        // or erased with J
        assert_eq!(tokens_shown(&format!("{token}\r\x1b[J")), vec![token]);
        assert!(tokens_shown("no token here\x1b[2K").is_empty());
    }

    #[test]
    fn links_modes_and_erases() {
        let raw = "\x1b]8;id=1;https://x.example/a\x07link\x1b]8;;\x07 here\x1b[?2004h\x1b(B\r\nline two\x1b[5D\x1b[K!";
        assert_eq!(screen(raw), "link here\nlin!");
        assert_eq!(screen("abc\r\ndef\x1b[2Jxyz"), "   xyz");
        assert_eq!(screen("one\r\ntwo\x1b[5A\rONE"), "ONE\ntwo"); // no higher than the top line
    }

    #[test]
    fn a_failure_says_what_claude_showed_but_never_a_token() {
        let text = "Your OAuth token:\nsk-ant-oat01-secretsecretsecretsecret\nPress Enter to continue";
        assert_eq!(last_said(text), "sk-ant-… / Press Enter to continue");
    }
}

/// Against the real `claude setup-token`, with a made-up code: `cargo test -- --ignored real_setup_token`.
/// It gives a link that works anywhere, then says Claude refused the code, in words read off the screen.
#[cfg(test)]
#[test]
#[ignore]
fn real_setup_token() {
    let s = SignIns::new(None);
    let got = s.start("t1", Duration::from_secs(60)).unwrap();
    assert!(got.url.contains("platform.claude.com"));
    let keep = tempfile::tempdir().unwrap();
    // as long as a real one (James's was 92 characters): sent with its Enter in one go, it never submitted
    let code = format!("{}#{}", "fakecode".repeat(6), "fakestate1".repeat(4));
    let code = code.as_str();
    let err = s.code("t1", code, Duration::from_secs(60), Some(keep.path())).unwrap_err();
    let kept = fs::read_to_string(keep.path().join("signin-last.log")).unwrap();
    assert!(kept.contains("OAuth error") && !kept.contains(code)); // (Claude Code shows the code masked anyway)
    println!("{err}");
    assert!(err.to_string().contains("didn't take that code") && err.to_string().contains("Request failed"));
}
