//! `rota serve`: the extension its node runs. Anything else: the command line for adding this machine's
//! accounts (`rota add work`), which only this machine's user can do.

use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => {
            if let Err(e) = rota::serve::serve() {
                eprintln!("rota: {e:#}");
                exit(1);
            }
        }
        Some(_) => exit(rota::cli::main(&args)),
        None => exit(rota::cli::main(&["help".into()])),
    }
}
