//! Rota's accounts as the apps draw them (MB-276; Pointman thread 649 [3966], [3970]): `/rota usage` and
//! the telemetry panel. A line for each account, a chart of each one's 5-hour and weekly use, and buttons
//! that move new runs. Core posts `text` for agents and older apps and `blocks` for the apps (blocks
//! version 1: text, a chart/v1 spec, actions); the panel is the first chart.

use serde_json::{json, Value};

use crate::util::local_time;

const MAX_BUTTONS: usize = 6;

/// A summary's Codex accounts (James, Pointman thread 3359 [7694]): none on a machine without Codex.
fn codex_accounts(summary: &Value) -> Vec<Value> {
    summary["codex"]["accounts"].as_array().cloned().unwrap_or_default()
}

/// Each account's windows as chart rows: [account, window, fraction used].
fn rows<'a>(accounts: &'a [Value], name: impl Fn(&Value) -> Value + 'a) -> impl Iterator<Item = Value> + 'a {
    accounts.iter().flat_map(move |a| {
        let shown = name(a);
        [("five_hour", "5-hour"), ("seven_day", "weekly")].into_iter().filter_map(move |(key, window)| {
            a[key]["used"].as_f64().map(|used| json!([shown.clone(), window, (used * 10.0).round() / 1000.0]))
        })
    })
}

/// `{text, blocks}` for a summary (`Rota::summary`), with what was just done first when there's a message.
/// Codex's accounts come after Claude's, under their own heading.
pub fn view(summary: &Value, message: Option<&str>) -> (String, Value) {
    let accounts = summary["accounts"].as_array().cloned().unwrap_or_default();
    let codex = codex_accounts(summary);
    let current = summary["current"].as_str();
    let mut lines: Vec<String> = message.filter(|m| !m.is_empty()).map(|m| format!("{m}\n")).into_iter().collect();
    if !codex.is_empty() {
        lines.push("**Claude**".into());
    }
    for a in &accounts {
        lines.push(format!("- **{}** · {}", a["label"].as_str().unwrap_or("?"), a["line"].as_str().unwrap_or("")));
    }
    if accounts.is_empty() {
        lines.push("No Claude accounts yet: `/rota add` signs one in.".into());
    }
    if !codex.is_empty() {
        lines.push("\n**Codex**".into());
        for a in &codex {
            lines.push(format!("- **{}** · {}", a["label"].as_str().unwrap_or("?"), a["line"].as_str().unwrap_or("")));
        }
    }
    let text = lines.join("\n");
    let mut blocks = vec![json!({"type": "text", "markdown": text})];
    let mut chart_rows: Vec<Value> = rows(&accounts, |a| a["label"].clone()).collect();
    // Codex's own login is `default` too: its accounts are named as Codex's
    chart_rows.extend(rows(&codex, |a| json!(format!("Codex {}", a["label"].as_str().unwrap_or("?")))));
    if !chart_rows.is_empty() {
        blocks.push(json!({"type": "chart", "spec": chart(chart_rows, current, !codex.is_empty())}));
    }
    let mut buttons = vec![];
    if accounts.len() > 1 {
        buttons.push(json!({"title": "Swap now", "command": "rota.swap", "params": {}}));
    }
    for a in &accounts {
        let label = a["label"].as_str().unwrap_or_default();
        if buttons.len() < MAX_BUTTONS && Some(label) != current {
            buttons.push(json!({"title": format!("Start runs on {label}"), "command": "rota.swap",
                                "params": {"to": label}}));
        }
    }
    if !buttons.is_empty() {
        blocks.push(json!({"type": "actions", "buttons": buttons}));
    }
    (text, Value::Array(blocks))
}

/// One account in the pane: its name with what the meters don't say, then a meter for each window.
fn account_block(a: &Value, threshold: f64, now: f64) -> Value {
    let label = a["label"].as_str().unwrap_or("?");
    let id = a["id"].as_str().unwrap_or(label);
    // what the meters don't say: runs start here, credits, a plan, a refused token
    let rest: Vec<&str> = a["line"]
        .as_str()
        .unwrap_or_default()
        .split(" · ")
        .filter(|bit| !bit.is_empty() && !bit.starts_with("5h ") && !bit.starts_with("week "))
        .collect();
    let mut children = vec![json!({"type": "heading", "id": format!("name:{id}"), "level": 3, "text": label,
                                    "subtitle": rest.join(" · ")})];
    for (key, name, short) in [("five_hour", "5-hour", "5h"), ("seven_day", "Weekly", "week")] {
        let Some(used) = a[key]["used"].as_f64() else { continue };
        let resets = a[key]["resets"].as_f64().filter(|r| *r != 0.0);
        let (value, note) = match resets {
            Some(r) if now >= r => (0.0, "reset".to_string()),
            Some(r) => {
                (used, format!("resets {}", local_time(r, if r - now < 20.0 * 3600.0 { "%H:%M" } else { "%a %H:%M" })))
            }
            None => (used, String::new()),
        };
        let tone = if value >= threshold {
            "bad"
        } else if value >= threshold - 20.0 {
            "warn"
        } else {
            "good"
        };
        children.push(json!({"type": "meter", "id": format!("{short}:{id}"), "label": name,
                             "value": (value * 10.0).round() / 10.0, "max": 100, "tone": tone, "note": note}));
    }
    json!({"type": "stack", "id": format!("account:{id}"), "axis": "v", "gap": 4, "children": children})
}

/// Rota's pane (Pointman's docs/dev/panes.md): the same accounts as blocks, a meter for each window, and
/// the buttons that move new runs (each a tap on `actions`, its id the button's), then Codex's accounts
/// under their own heading. Shown whole each time; the SDK sends only what changed, so a meter moves in
/// place.
pub fn pane(summary: &Value, now: f64) -> Value {
    let accounts = summary["accounts"].as_array().cloned().unwrap_or_default();
    let codex = codex_accounts(summary);
    let current = summary["current"].as_str();
    let threshold = summary["threshold"].as_f64().unwrap_or(90.0);
    let subtitle = match current {
        Some(c) => format!("New runs start on {c} · it swaps at {threshold:.0}%"),
        None => "No account picked yet".to_string(),
    };
    let mut blocks =
        vec![json!({"type": "heading", "id": "head", "level": 1, "text": "Claude accounts", "subtitle": subtitle})];
    if accounts.is_empty() {
        blocks.push(json!({"type": "empty", "id": "none", "title": "No Claude accounts yet",
                           "note": "`/rota add` signs one in."}));
    }
    blocks.extend(accounts.iter().map(|a| account_block(a, threshold, now)));
    let mut buttons = vec![];
    if accounts.len() > 1 {
        buttons.push(json!({"id": "swap", "title": "Swap now"}));
    }
    for a in &accounts {
        let label = a["label"].as_str().unwrap_or_default();
        let id = a["id"].as_str().unwrap_or(label);
        if buttons.len() < MAX_BUTTONS && Some(label) != current {
            buttons.push(json!({"id": format!("use:{id}"), "title": format!("Start runs on {label}")}));
        }
    }
    if !buttons.is_empty() {
        blocks.push(json!({"type": "actions", "id": "actions", "buttons": buttons}));
    }
    if !codex.is_empty() {
        blocks.push(json!({"type": "heading", "id": "codex", "level": 2, "text": "Codex accounts",
                           "subtitle": "As Codex's /status shows them"}));
        blocks.extend(codex.iter().map(|a| account_block(a, threshold, now)));
    }
    Value::Array(blocks)
}

/// Each account's windows as grouped bars, as fractions drawn as percentages; the title says which one new
/// runs start on (James, Pointman thread 649 [3985]: "usage and the currently active account").
fn chart(rows: Vec<Value>, current: Option<&str>, codex: bool) -> Value {
    let (what, runs) = if codex { ("Claude and Codex accounts", "Claude runs") } else { ("Claude accounts", "runs") };
    let title = current.map_or(what.to_string(), |c| format!("{what} · {runs} start on {c}"));
    json!({"type": "bar", "title": title,
           "description": "How full each account's 5-hour and weekly windows are",
           "data": {"columns": ["account", "window", "used"], "rows": rows},
           "encoding": {"x": {"field": "account", "type": "nominal", "label": "Account"},
                        "y": {"field": "used", "type": "quantitative", "label": "Used",
                              "format": {"style": "percent"}},
                        "series": {"field": "window", "type": "nominal"}},
           "options": {"grouped": true}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> Value {
        json!({"current": "work", "accounts": [
            {"label": "work", "line": "5h 62% (resets 14:00) · week 31% · runs start here",
             "five_hour": {"used": 62.0, "resets": 1.0}, "seven_day": {"used": 31.0}},
            {"label": "spare", "line": "not checked yet"},
            {"label": "old", "line": "5h 99%", "five_hour": {"used": 99.5}}]})
    }

    #[test]
    fn a_line_each_a_chart_of_what_is_known_and_buttons_to_move_runs() {
        let (text, blocks) = view(&summary(), Some("Runs start on spare now."));
        assert!(text.starts_with("Runs start on spare now.\n\n- **work** · 5h 62%"));
        assert!(text.contains("- **spare** · not checked yet"));
        let blocks = blocks.as_array().unwrap();
        assert_eq!(blocks[0], json!({"type": "text", "markdown": text}));
        let spec = &blocks[1]["spec"];
        assert_eq!(
            spec["data"]["rows"],
            json!([["work", "5-hour", 0.62], ["work", "weekly", 0.31], ["old", "5-hour", 0.995]])
        );
        assert_eq!(spec["encoding"]["y"]["format"]["style"], "percent");
        assert_eq!(spec["title"], "Claude accounts · runs start on work");
        // not the one runs start on
        let titles: Vec<&str> =
            blocks[2]["buttons"].as_array().unwrap().iter().map(|b| b["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["Swap now", "Start runs on spare", "Start runs on old"]);
        assert_eq!(blocks[2]["buttons"][1]["params"], json!({"to": "spare"}));
    }

    #[test]
    fn the_pane_has_a_meter_for_each_window_and_buttons_that_move_runs() {
        let mut s = summary();
        s["threshold"] = json!(90.0);
        s["accounts"][0]["id"] = json!("work");
        s["accounts"][0]["five_hour"]["resets"] = json!(4102444800.0);
        s["accounts"][0]["seven_day"]["resets"] = json!(1.0); // passed: reset
        s["accounts"][0]["line"] = json!("5h 62% (resets 14:00) · week 31% · runs start here");
        let blocks = pane(&s, 1000.0);
        let blocks = blocks.as_array().unwrap();
        assert_eq!(blocks[0]["subtitle"], "New runs start on work · it swaps at 90%");
        let work = &blocks[1];
        assert_eq!(work["id"], "account:work");
        assert_eq!(
            work["children"][0],
            json!({"type": "heading", "id": "name:work", "level": 3, "text": "work",
                                               "subtitle": "runs start here"})
        );
        let five = &work["children"][1];
        assert_eq!(
            (five["id"].as_str(), five["value"].as_f64(), five["tone"].as_str()),
            (Some("5h:work"), Some(62.0), Some("good"))
        );
        assert!(five["note"].as_str().unwrap().starts_with("resets "));
        assert_eq!(
            (work["children"][2]["value"].as_f64(), work["children"][2]["note"].as_str()),
            (Some(0.0), Some("reset"))
        );
        // no windows known yet: its line says so, and no meters
        assert_eq!(blocks[2]["children"].as_array().unwrap().len(), 1);
        assert_eq!(blocks[2]["children"][0]["subtitle"], "not checked yet");
        assert_eq!(blocks[3]["children"][1]["tone"], "bad"); // old, at 99.5%
        let ids: Vec<&str> =
            blocks[4]["buttons"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["swap", "use:spare", "use:old"]);
        let none = pane(&json!({"current": null, "accounts": []}), 0.0);
        assert_eq!(none[1]["type"], "empty");
    }

    /// James, Pointman thread 3359 [7694]: Codex's accounts after Claude's, in the text, the chart and the pane.
    #[test]
    fn codex_accounts_come_after_claudes() {
        let mut s = summary();
        s["accounts"][0]["id"] = json!("work");
        s["codex"] = json!({"current": null, "accounts": [
            {"label": "default", "id": "codex.default", "line": "week 1% (resets Thu 15:41) · Business plan",
             "seven_day": {"used": 1.0, "resets": 4102444800.0}}]});
        let (text, blocks) = view(&s, None);
        assert!(text.starts_with("**Claude**\n- **work** · "), "{text}");
        assert!(text.ends_with("\n\n**Codex**\n- **default** · week 1% (resets Thu 15:41) · Business plan"), "{text}");
        let spec = &blocks[1]["spec"];
        assert_eq!(spec["data"]["rows"][3], json!(["Codex default", "weekly", 0.01]));
        assert_eq!(spec["title"], "Claude and Codex accounts · Claude runs start on work");
        // Codex's accounts have no buttons: the node runs Claude Code only
        assert_eq!(blocks[2]["buttons"].as_array().unwrap().len(), 3);
        let pane = pane(&s, 1000.0);
        let pane = pane.as_array().unwrap();
        let at = pane.iter().position(|b| b["id"] == "codex").unwrap();
        assert_eq!(pane[at - 1]["id"], "actions");
        assert_eq!(pane[at + 1]["id"], "account:codex.default");
        assert_eq!(pane[at + 1]["children"][0]["subtitle"], "Business plan");
        assert_eq!(pane[at + 1]["children"][1]["id"], "week:codex.default");
        // and none of it on a machine without Codex
        let (text, _) = view(&summary(), None);
        assert!(!text.contains("Codex") && !text.contains("**Claude**"));
    }

    #[test]
    fn no_accounts_says_how_to_add_one() {
        let (text, blocks) = view(&json!({"current": null, "accounts": []}), None);
        assert_eq!(text, "No Claude accounts yet: `/rota add` signs one in.");
        assert_eq!(blocks.as_array().unwrap().len(), 1);
    }
}
