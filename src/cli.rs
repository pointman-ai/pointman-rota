//! `rota add|remove|hold|use|list`: this machine's accounts, kept where only its user can read them. A
//! token is pasted here, never sent through the board.

use std::io::{self, BufRead, Write};

use crate::accounts::{self, Account, DEFAULT};
use crate::codex;

const USAGE: &str =
    "Accounts are best kept in the board's vault as rota.<label> (from the apps, or `pointman secrets add`).
On this machine only:
rota add <label>        add an account: paste the token `claude setup-token` printed
rota remove <label>     take one out
rota hold <label>       keep runs off it for now, a vault one too (use: put it back)
rota list               the accounts in this machine's file (never their tokens)
rota codex add <label>  a Codex account: signed in with `codex login` in a Codex home of its own";

/// A line from stdin, not shown as it's typed when that's a terminal (getpass).
fn secret_line(prompt: &str) -> String {
    let tty = unsafe { libc::isatty(0) } == 1;
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if tty {
        eprint!("{prompt}");
        let _ = io::stderr().flush();
        // SAFETY: termios read and written back on stdin, which is a terminal
        unsafe {
            libc::tcgetattr(0, &mut saved);
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &quiet);
        }
    }
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    if tty {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        eprintln!();
    }
    line
}

pub fn main(argv: &[String]) -> i32 {
    if argv.first().map(String::as_str) == Some("codex") {
        return codex_main(&argv[1..]);
    }
    let cmd = argv.first().map_or("", String::as_str);
    let label = argv.get(1).map(|l| l.to_lowercase()).unwrap_or_default();
    let mut store = accounts::load_store(None);
    if cmd == "list" {
        println!("{DEFAULT}  (this machine's own Claude login)");
        for a in &store.accounts {
            println!("{}{}", a.label, if a.held() { "  (held)" } else { "" });
        }
        return 0;
    }
    if !["add", "remove", "hold", "use"].contains(&cmd) || label.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }
    if label == DEFAULT || !accounts::is_label(&label) {
        eprintln!("pick another label: lowercase letters, digits, . _ -, and not '{DEFAULT}'");
        return 2;
    }
    let saved = match cmd {
        "add" => {
            let token = secret_line(&format!("{label}'s token (from `claude setup-token`): ")).trim().to_string();
            if !accounts::is_token(&token) {
                eprintln!(
                    "that isn't a setup token (sk-ant-oat01-…): run `claude setup-token` signed in as that account"
                );
                return 1;
            }
            store.accounts.retain(|a| a.label != label);
            store.accounts.push(Account { token: Some(token), ..Account::new(&label) });
            accounts::save_store(&store, None).map(|_| {
                println!("added {label}. Rota checks it within a minute; `pointman health` shows how full it is.")
            })
        }
        // one kept in the vault is held here by its label
        "hold" => accounts::hold(&label, true, None).map(|_| println!("runs stay off {label}")),
        "use" => accounts::hold(&label, false, None).map(|_| println!("{label} is back in turn")),
        _ if !store.accounts.iter().any(|a| a.label == label) => {
            eprintln!("no account '{label}' in this machine's file");
            return 1;
        }
        _ => {
            store.accounts.retain(|a| a.label != label);
            accounts::save_store(&store, None).map(|_| println!("removed {label}"))
        }
    };
    match saved {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("couldn't write {}: {e}", accounts::store_path().display());
            1
        }
    }
}

fn codex_main(argv: &[String]) -> i32 {
    let label = argv.get(1).map(|l| l.to_lowercase()).unwrap_or_default();
    if argv.first().map(String::as_str) != Some("add") || !accounts::is_label(&label) || label == DEFAULT {
        eprintln!("rota codex add <label>   (lowercase letters, digits, . _ -; not default)");
        return 2;
    }
    match codex::add(&label, None, None) {
        Ok(said) => {
            println!("{said}");
            0
        }
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    }
}
