//! Rota's accounts as the apps draw them (MB-276; Pointman thread 649 [3966], [3970]): `/rota usage` and
//! the telemetry panel. A line for each account, a chart of each one's 5-hour and weekly use, and buttons
//! that move new runs. Core posts `text` for agents and older apps and `blocks` for the apps (blocks
//! version 1: text, a chart/v1 spec, actions); the panel is the first chart.

use serde_json::{json, Value};

const MAX_BUTTONS: usize = 6;

/// `{text, blocks}` for a summary (`Rota::summary`), with what was just done first when there's a message.
pub fn view(summary: &Value, message: Option<&str>) -> (String, Value) {
    let accounts = summary["accounts"].as_array().cloned().unwrap_or_default();
    let current = summary["current"].as_str();
    let mut lines: Vec<String> = message.filter(|m| !m.is_empty()).map(|m| format!("{m}\n")).into_iter().collect();
    for a in &accounts {
        lines.push(format!("- **{}** · {}", a["label"].as_str().unwrap_or("?"), a["line"].as_str().unwrap_or("")));
    }
    if accounts.is_empty() {
        lines.push("No Claude accounts yet: `/rota add` signs one in.".into());
    }
    let text = lines.join("\n");
    let mut blocks = vec![json!({"type": "text", "markdown": text})];
    let rows: Vec<Value> = accounts
        .iter()
        .flat_map(|a| {
            [("five_hour", "5-hour"), ("seven_day", "weekly")].into_iter().filter_map(move |(key, name)| {
                a[key]["used"].as_f64().map(|used| json!([a["label"], name, (used * 10.0).round() / 1000.0]))
            })
        })
        .collect();
    if !rows.is_empty() {
        blocks.push(json!({"type": "chart", "spec": chart(rows, current)}));
    }
    let mut buttons = vec![];
    if accounts.len() > 1 {
        buttons.push(json!({"title": "Swap now", "command": "rota.swap", "params": {}}));
    }
    for a in &accounts {
        let label = a["label"].as_str().unwrap_or_default();
        if buttons.len() < MAX_BUTTONS && Some(label) != current && a["held"] != true {
            buttons.push(json!({"title": format!("Start runs on {label}"), "command": "rota.swap",
                                "params": {"to": label}}));
        }
    }
    if !buttons.is_empty() {
        blocks.push(json!({"type": "actions", "buttons": buttons}));
    }
    (text, Value::Array(blocks))
}

/// Each account's windows as grouped bars, as fractions drawn as percentages; the title says which one new
/// runs start on (James, Pointman thread 649 [3985]: "usage and the currently active account").
fn chart(rows: Vec<Value>, current: Option<&str>) -> Value {
    let title = current.map_or("Claude accounts".to_string(), |c| format!("Claude accounts · runs start on {c}"));
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
            {"label": "old", "line": "held", "held": true, "five_hour": {"used": 99.5}}]})
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
        // not the one runs start on, nor a held one
        let titles: Vec<&str> =
            blocks[2]["buttons"].as_array().unwrap().iter().map(|b| b["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["Swap now", "Start runs on spare"]);
        assert_eq!(blocks[2]["buttons"][1]["params"], json!({"to": "spare"}));
    }

    #[test]
    fn no_accounts_says_how_to_add_one() {
        let (text, blocks) = view(&json!({"current": null, "accounts": []}), None);
        assert_eq!(text, "No Claude accounts yet: `/rota add` signs one in.");
        assert_eq!(blocks.as_array().unwrap().len(), 1);
    }
}
