//! Rota: Claude accounts taken in turns. Each agent run a node starts goes on the account with the most
//! room left, and Grok's and ZCode's keys are taken in turns too, moving on after a limit. One binary: `rota serve` is the extension its node runs, and `rota add|remove|list`
//! and `rota codex add` are the command line for this machine's own accounts.

pub mod accounts;
pub mod cli;
pub mod codex;
pub mod keys;
pub mod probe;
pub mod serve;
pub mod signin;
pub mod util;
pub mod view;
