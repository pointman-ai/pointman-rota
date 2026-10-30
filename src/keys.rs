//! Several keys for a client that runs on an API key (EXT-113; James, Pointman thread 8028 [27934]): Grok and
//! ZCode. Neither has a usage number to read, so rota moves on when one fails.
//!
//! - **The keys** are the client's own vault items: `<client>.key` (its `/grok add`, shown as `default`) and any
//!   `<client>.key.<label>` beside it (`grok.key.work`). Rota only lists their names. The node opens the one rota
//!   names (`key_label`, node 0.4.5), and a run never gets a key from rota itself.
//! - **The rule:** runs stay on a key until a run on it ends at a limit, which rota tells from the run's error.
//!   That key then sits out `key_rest_minutes` (60), and runs move to the next key in order that isn't sitting
//!   out. With every key sitting out, runs go to the one back soonest. A run that ends well clears its key.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::accounts::DEFAULT;
use crate::util;

/// The clients rota takes keys in turns for: their id, and what people call them.
pub const CLIENTS: [(&str, &str); 2] = [("grok", "Grok"), ("zcode", "ZCode")];

/// Words a run's error has when its key hit a limit. Grok's are its manifest's `[client.limits]`; the rest are
/// the node's for any client's usage or spend limit (limits.rs), which ZCode's GLM Coding Plan errors use
/// ("Usage limit reached for 5 hour"). The node doesn't pass a client's own words to hooks, so they're here.
static LIMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)rate[ _-]?limit|too many requests|quota exceeded|usage limit|hit your [^.·\n]{0,60}limit|spend limit|out of (extra )?usage|credit limit|out of credits|insufficient balance|(5-hour|weekly|session) limit reached",
    )
    .unwrap()
});

/// Whether rota takes keys in turns for this client.
pub fn takes(client: &str) -> bool {
    CLIENTS.iter().any(|(id, _)| *id == client)
}

/// What people call a client: Grok.
pub fn name(client: &str) -> &str {
    CLIENTS.iter().find(|(id, _)| *id == client).map_or(client, |(_, name)| name)
}

/// A key's label as the node takes it (`key_label`): lowercase letters, digits and `-`, up to 32.
pub fn is_label(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A client's keys from the vault's names under `<client>.`, in the order runs move through them: its own key
/// (`default`) first, then the labelled ones by label.
pub fn labels(client: &str, names: &[String]) -> Vec<String> {
    let own = format!("{client}.key");
    let mut out: Vec<String> = names
        .iter()
        .filter_map(|n| n.strip_prefix(&format!("{own}.")))
        .filter(|l| *l != DEFAULT && is_label(l))
        .map(String::from)
        .collect();
    out.sort();
    out.dedup();
    if names.contains(&own) {
        out.insert(0, DEFAULT.into());
    }
    out
}

/// Whether a run's error says its key hit a limit.
pub fn said_limit(error: &str) -> bool {
    LIMIT.is_match(error)
}

/// A key a run ended at a limit on: until when it sits out, and the client's words.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Limited {
    pub until: f64,
    pub said: String,
}

/// What rota keeps of one client's keys.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Keys {
    /// The key runs start on.
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub limited: BTreeMap<String, Limited>,
}

impl Keys {
    fn resting(&self, label: &str, now: f64) -> bool {
        self.limited.get(label).is_some_and(|l| now < l.until)
    }

    /// A run on `label` ended at a limit: it sits out `rest` seconds.
    pub fn hit_limit(&mut self, label: &str, error: &str, rest: f64, now: f64) {
        let said = error.trim().chars().take(200).collect();
        self.limited.insert(label.to_string(), Limited { until: now + rest, said });
    }

    /// A run on `label` ended well: it takes runs again.
    pub fn worked(&mut self, label: &str) {
        self.limited.remove(label);
    }
}

/// The key a new run starts on, of `labels` (in order): the current one while it isn't sitting out, else the
/// next after it that isn't, else the one back soonest. None: the client has no key here.
pub fn choose(labels: &[String], keys: &Keys, now: f64) -> Option<String> {
    let at = keys.current.as_ref().and_then(|c| labels.iter().position(|l| l == c));
    if let Some(i) = at.filter(|i| !keys.resting(&labels[*i], now)) {
        return Some(labels[i].clone());
    }
    let start = at.map_or(0, |i| i + 1);
    let next = (0..labels.len()).map(|n| &labels[(start + n) % labels.len()]).find(|l| !keys.resting(l, now));
    next.or_else(|| {
        labels.iter().min_by(|a, b| {
            let until = |l: &str| keys.limited.get(l).map_or(0.0, |x| x.until);
            until(a).partial_cmp(&until(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
    })
    .cloned()
}

/// A key's line for `/rota usage` and the pane: "runs start here", "hit a limit at 14:05 · back at 15:05".
pub fn describe(label: &str, keys: &Keys, now: f64) -> String {
    let mut bits = vec![];
    if let Some(l) = keys.limited.get(label).filter(|l| now < l.until) {
        let at = util::local_time(l.until, "%H:%M");
        bits.push(format!("sat out after a limit · back at {at}"));
    }
    if keys.current.as_deref() == Some(label) {
        bits.push("runs start here".into());
    }
    if bits.is_empty() {
        bits.push("ready".into());
    }
    bits.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_clients_keys_are_its_own_and_its_labelled_ones() {
        let names =
            all(&["grok.key.work", "grok.key", "grok.key.Bad", "grok.key.default", "grok.other", "grok.key.b-2"]);
        assert_eq!(labels("grok", &names), ["default", "b-2", "work"]);
        assert_eq!(labels("grok", &all(&["grok.key.work"])), ["work"]);
        assert!(labels("zcode", &names).is_empty());
    }

    #[test]
    fn limit_words() {
        for e in [
            "429 Too Many Requests",
            "xAI: rate limit exceeded for this key",
            "Usage limit reached for 5 hour. Your limit will reset at 2026-10-30 18:00:00",
            "Insufficient balance or no resource package. Please recharge.",
            "You've hit your usage limit",
        ] {
            assert!(said_limit(e), "{e}");
        }
        assert!(!said_limit("the session ended without a result"));
        assert!(!said_limit("No such file or directory"));
    }

    #[test]
    fn runs_stay_on_a_key_until_it_hits_a_limit_then_move_to_the_next() {
        let labels = all(&["default", "a", "b"]);
        let mut keys = Keys::default();
        assert_eq!(choose(&labels, &keys, 0.0).as_deref(), Some("default"), "first pick: the first key");
        keys.current = Some("a".into());
        assert_eq!(choose(&labels, &keys, 0.0).as_deref(), Some("a"), "sticky");
        keys.hit_limit("a", "rate limit", 3600.0, 0.0);
        assert_eq!(choose(&labels, &keys, 10.0).as_deref(), Some("b"), "the next one after it");
        keys.current = Some("b".into());
        keys.hit_limit("b", "rate limit", 3600.0, 100.0);
        assert_eq!(choose(&labels, &keys, 200.0).as_deref(), Some("default"), "wraps round");
        keys.hit_limit("default", "rate limit", 3600.0, 200.0);
        assert_eq!(choose(&labels, &keys, 300.0).as_deref(), Some("a"), "all sitting out: the one back soonest");
        assert_eq!(choose(&labels, &keys, 3601.0).as_deref(), Some("a"), "a is back");
        keys.worked("b");
        assert_eq!(choose(&labels, &keys, 300.0).as_deref(), Some("b"), "a run on b worked: it's current and ready");
    }

    #[test]
    fn one_key_or_none() {
        let mut keys = Keys::default();
        assert_eq!(choose(&[], &keys, 0.0), None);
        keys.hit_limit("default", "rate limit", 3600.0, 0.0);
        assert_eq!(choose(&all(&["default"]), &keys, 10.0).as_deref(), Some("default"), "nowhere else to go");
        keys.current = Some("gone".into());
        assert_eq!(choose(&all(&["work"]), &keys, 10.0).as_deref(), Some("work"), "a key that's gone: the first");
    }
}
