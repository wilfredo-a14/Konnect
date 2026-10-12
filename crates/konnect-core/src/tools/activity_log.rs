//! Announcing tool calls to KiCad's API Activity panel.
//!
//! The fredo fork shows every IPC request it answers in a dockable panel. A
//! `LogActivity` start and end around each tool call adds the call itself,
//! with the requests it makes nested underneath. KiCad sees only what is sent
//! here, so arguments are redacted first.
//!
//! Best effort throughout: a stock KiCad answers `AS_UNHANDLED`, and with no
//! KiCad the request fails at once; neither affects the tool call.

use std::time::Duration;

use konnect_ipc::gen::kiapi::common::commands::{ActivityPhase, LogActivity};
use konnect_ipc::KiCadIpcClient;
use serde_json::Value;

use super::live_schematic::endpoint;

/// The start is awaited so it reaches KiCad before the call's own requests,
/// which then nest under it; a busy editor must not hold the call up longer.
const SEND_TIMEOUT: Duration = Duration::from_millis(300);

const MAX_DETAIL_BYTES: usize = 8 * 1024;

/// Argument names whose values never leave Konnect (AUT-032).
const SECRET_MARKERS: [&str; 7] = [
    "key",
    "token",
    "secret",
    "password",
    "authorization",
    "cookie",
    "credential",
];

/// How many targets a title names before "and N more".
const MAX_TARGETS: usize = 6;

/// A plain-language title for a tool call, naming what it acts on:
/// "Set component placements: R101 → (141.6, 115), TP801 → (163.2, 111.8)".
/// Paths and secrets are left out; the panel's detail shows the arguments.
pub fn describe(tool: &str, args: &Value) -> String {
    let words = tool.replace('_', " ");
    let mut verb = String::new();
    let mut chars = words.chars();
    if let Some(first) = chars.next() {
        verb.extend(first.to_uppercase());
        verb.push_str(chars.as_str());
    }

    let mut targets = Vec::new();
    collect_targets(args, &mut targets);
    if targets.is_empty() {
        if let Some(name) = [
            "net", "net_name", "name", "label", "text", "layer", "lib_id",
        ]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        {
            targets.push(name.to_string());
        }
    }
    if targets.is_empty() {
        return verb;
    }

    let shown = targets.len().min(MAX_TARGETS);
    let mut title = format!("{verb}: {}", targets[..shown].join(", "));
    if targets.len() > shown {
        title.push_str(&format!(" and {} more", targets.len() - shown));
    }
    title
}

/// Every object in `value` that names a part, as "R101 → (1, 2) @ 90° = 10k on B.Cu",
/// plus any plain list of references.
fn collect_targets(value: &Value, targets: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            let reference = ["reference", "ref", "designator", "new_reference"]
                .iter()
                .find_map(|key| fields.get(*key).and_then(Value::as_str));
            if let Some(reference) = reference {
                targets.push(target_text(reference, fields));
            }
            for (key, field) in fields {
                if is_secret(key) {
                    continue;
                }
                match field {
                    Value::Array(items) if items.iter().all(Value::is_string) => {
                        if ["references", "refs", "designators"].contains(&key.as_str()) {
                            targets
                                .extend(items.iter().filter_map(Value::as_str).map(str::to_string));
                        }
                    }
                    Value::Array(_) | Value::Object(_)
                        if reference.is_none() || field.is_array() =>
                    {
                        collect_targets(field, targets)
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_targets(item, targets)),
        _ => {}
    }
}

fn target_text(reference: &str, fields: &serde_json::Map<String, Value>) -> String {
    let number = |key: &str| fields.get(key).and_then(Value::as_f64);
    let mut text = reference.to_string();
    if let (Some(x), Some(y)) = (number("x"), number("y")) {
        text.push_str(&format!(" → ({}, {})", short(x), short(y)));
    }
    if let Some(angle) = number("rotation").or_else(|| number("angle")) {
        text.push_str(&format!(" @ {}°", short(angle)));
    }
    if let Some(value) = fields.get("value").and_then(Value::as_str) {
        text.push_str(&format!(" = {value}"));
    }
    if let Some(side) = ["layer", "side"]
        .iter()
        .find_map(|key| fields.get(*key).and_then(Value::as_str))
    {
        text.push_str(&format!(" on {side}"));
    }
    text
}

/// A coordinate without float noise: 141.6, not 141.60000000000002.
fn short(value: f64) -> String {
    let rounded = (value * 1000.0).round() / 1000.0;
    format!("{rounded}")
}

pub async fn start(configured_address: &str, call_id: &str, tool: &str, args: &Value) {
    let detail = serde_json::to_string_pretty(&redact(args)).unwrap_or_default();
    send(
        configured_address,
        LogActivity {
            call_id: call_id.to_string(),
            phase: ActivityPhase::ApStart as i32,
            title: tool.to_string(),
            detail: truncate(&detail, MAX_DETAIL_BYTES),
            failed: false,
            duration_ms: 0,
        },
    )
    .await;
}

pub async fn end(
    configured_address: &str,
    call_id: &str,
    tool: &str,
    failed: bool,
    duration_ms: u64,
    summary: &str,
) {
    send(
        configured_address,
        LogActivity {
            call_id: call_id.to_string(),
            phase: ActivityPhase::ApEnd as i32,
            title: tool.to_string(),
            detail: truncate(summary, MAX_DETAIL_BYTES),
            failed,
            duration_ms,
        },
    )
    .await;
}

/// The server resolves its KiCad address once at startup, so an empty one
/// here means no KiCad to report to. It is never auto-detected: a test
/// context with no address would otherwise reach whatever KiCad is running.
async fn send(configured_address: &str, activity: LogActivity) {
    let address = configured_address.trim().to_string();
    if address.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        let _ = KiCadIpcClient::new(address).log_activity(&activity, SEND_TIMEOUT);
    })
    .await;
}

/// Client name for Claude Code's hooks. Each hook is its own process, so a
/// fixed name lets KiCad pair a command's end with its start.
const HOOK_CLIENT: &str = "claude-code";

/// Post one Claude Code hook event (its stdin JSON) to the API Activity panel:
/// PreToolUse starts an operation, PostToolUse or PostToolUseFailure ends it.
/// Never fails; a hook must not disturb the command it reports.
pub fn post_claude_hook(input: &str) {
    let Ok(event) = serde_json::from_str::<Value>(input) else {
        return;
    };
    let Some(activity) = hook_activity(&event) else {
        return;
    };
    if let Some(address) = endpoint("") {
        let _ = KiCadIpcClient::new(address)
            .with_client_name(HOOK_CLIENT)
            .log_activity(&activity, SEND_TIMEOUT);
    }
}

fn hook_activity(event: &Value) -> Option<LogActivity> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let call_id = text(&event["tool_use_id"]);
    if call_id.is_empty() {
        return None;
    }
    let tool = text(&event["tool_name"]);
    let input = &event["tool_input"];
    let command = redact_command(input["command"].as_str().unwrap_or_default());
    let description = text(&input["description"]);
    let title = if tool == "Bash" {
        let summary = if description.is_empty() {
            command.lines().next().unwrap_or_default().to_string()
        } else {
            description
        };
        format!("$ {}", truncate_line(&summary, 100))
    } else {
        tool.clone()
    };

    let (phase, detail, failed) = match event["hook_event_name"].as_str()? {
        "PreToolUse" => {
            let cwd = text(&event["cwd"]);
            (
                ActivityPhase::ApStart,
                format!("cwd: {cwd}\n\n{command}"),
                false,
            )
        }
        "PostToolUse" => {
            let response = &event["tool_response"];
            let mut output = String::new();
            for stream in ["stdout", "stderr"] {
                if let Some(content) = response[stream].as_str().filter(|c| !c.is_empty()) {
                    output.push_str(&format!(
                        "{stream}:\n{}\n",
                        tail(content, MAX_DETAIL_BYTES / 2)
                    ));
                }
            }
            (ActivityPhase::ApEnd, output, false)
        }
        "PostToolUseFailure" => (ActivityPhase::ApEnd, text(&event["error"]), true),
        _ => return None,
    };

    Some(LogActivity {
        call_id,
        phase: phase as i32,
        title,
        detail: truncate(&detail, MAX_DETAIL_BYTES),
        failed,
        // 0 asks KiCad to time the command from its start
        duration_ms: event["duration_ms"].as_u64().unwrap_or(0),
    })
}

/// `command` with the values of secret-looking assignments, flags and
/// bearer tokens replaced. Spacing and line breaks are kept.
pub(crate) fn redact_command(command: &str) -> String {
    command
        .split('\n')
        .map(|line| {
            let mut hide_next = false;
            line.split(' ')
                .map(|word| {
                    if std::mem::take(&mut hide_next) && !word.is_empty() {
                        return "<redacted>".to_string();
                    }
                    let bare = word.trim_start_matches('-');
                    if let Some((name, _)) = bare.split_once('=') {
                        if is_secret(name) {
                            let prefix = &word[..word.len() - bare.len()];
                            return format!("{prefix}{name}=<redacted>");
                        }
                        return word.to_string();
                    }
                    let flag = word.starts_with('-') && is_secret(bare);
                    let bearer = word
                        .trim_start_matches(['"', '\''])
                        .eq_ignore_ascii_case("bearer");
                    hide_next = flag || bearer;
                    word.to_string()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_line(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}...")
}

/// At most the last `max` bytes of `text`, cut on a character boundary.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("... ({start} earlier bytes)\n{}", &text[start..])
}

fn is_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_MARKERS.iter().any(|marker| name.contains(marker))
}

/// `value` with every secret-named field replaced, at any depth.
pub(crate) fn redact(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(name, field)| {
                    let shown = if is_secret(name) {
                        Value::String("<redacted>".to_string())
                    } else {
                        redact(field)
                    };
                    (name.clone(), shown)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

/// At most `max` bytes of `text`, cut on a character boundary.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... ({} more bytes)", &text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_named_arguments_are_redacted_at_any_depth() {
        let args = json!({
            "board": "/tmp/a.kicad_pcb",
            "api_key": "abc",
            "jlc": { "accessToken": "xyz", "parts": [{ "Client_Secret": "s", "mpn": "R1" }] }
        });
        assert_eq!(
            redact(&args),
            json!({
                "board": "/tmp/a.kicad_pcb",
                "api_key": "<redacted>",
                "jlc": { "accessToken": "<redacted>", "parts": [{ "Client_Secret": "<redacted>", "mpn": "R1" }] }
            })
        );
    }

    #[test]
    fn titles_name_the_parts_a_call_acts_on() {
        assert_eq!(
            describe(
                "set_component_placements",
                &json!({ "board": "/b.kicad_pcb", "placements": [
                    { "reference": "R101", "x": 141.60000000000002, "y": 115.0, "rotation": 0 },
                    { "reference": "TP801", "x": 163.2, "y": 111.8 }
                ]})
            ),
            "Set component placements: R101 → (141.6, 115) @ 0°, TP801 → (163.2, 111.8)"
        );
        assert_eq!(
            describe(
                "edit_component",
                &json!({ "board": "/b", "reference": "R101", "value": "10k" })
            ),
            "Edit component: R101 = 10k"
        );
        assert_eq!(
            describe("flip_component", &json!({ "references": ["R305", "R306"] })),
            "Flip component: R305, R306"
        );
        assert_eq!(describe("save_project", &json!({})), "Save project");
        assert_eq!(
            describe(
                "add_schematic_net_label",
                &json!({ "schematic": "/s", "text": "VBAT" })
            ),
            "Add schematic net label: VBAT"
        );
    }

    #[test]
    fn long_target_lists_are_cut_short() {
        let placements: Vec<Value> = (1..=9)
            .map(|i| json!({ "reference": format!("C{i}") }))
            .collect();
        assert_eq!(
            describe(
                "set_component_placements",
                &json!({ "placements": placements })
            ),
            "Set component placements: C1, C2, C3, C4, C5, C6 and 3 more"
        );
    }

    #[test]
    fn shell_secrets_are_redacted_and_spacing_is_kept() {
        let command = "JLC_API_KEY=abc123 curl -H \"Authorization: Bearer xyz\" --token t1  --board=a.kicad_pcb\nexport SECRET=s ok=1";
        assert_eq!(
            redact_command(command),
            "JLC_API_KEY=<redacted> curl -H \"Authorization: Bearer <redacted> --token <redacted>  --board=a.kicad_pcb\nexport SECRET=<redacted> ok=1"
        );
    }

    #[test]
    fn a_bash_hook_pair_becomes_one_start_and_one_end() {
        let pre = json!({
            "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "toolu_1",
            "cwd": "/w", "tool_input": { "command": "kicad-cli pcb drc a.kicad_pcb", "description": "Run DRC" }
        });
        let start = hook_activity(&pre).unwrap();
        assert_eq!(start.phase, ActivityPhase::ApStart as i32);
        assert_eq!(start.call_id, "toolu_1");
        assert_eq!(start.title, "$ Run DRC");
        assert_eq!(start.detail, "cwd: /w\n\nkicad-cli pcb drc a.kicad_pcb");

        let post = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "toolu_1",
            "tool_input": { "command": "kicad-cli pcb drc a.kicad_pcb", "description": "Run DRC" },
            "tool_response": { "stdout": "Saved DRC Report", "stderr": "", "interrupted": false },
            "duration_ms": 1500
        });
        let end = hook_activity(&post).unwrap();
        assert_eq!(end.phase, ActivityPhase::ApEnd as i32);
        assert!(!end.failed);
        assert_eq!(end.duration_ms, 1500);
        assert_eq!(end.detail, "stdout:\nSaved DRC Report\n");

        let failure = json!({
            "hook_event_name": "PostToolUseFailure", "tool_name": "Bash", "tool_use_id": "toolu_1",
            "tool_input": { "command": "false" }, "error": "Exit code 1"
        });
        let end = hook_activity(&failure).unwrap();
        assert!(end.failed);
        assert_eq!(end.title, "$ false");
        assert_eq!(end.duration_ms, 0);
    }

    #[test]
    fn events_without_a_tool_use_id_or_of_other_kinds_are_ignored() {
        assert!(
            hook_activity(&json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash" }))
                .is_none()
        );
        assert!(hook_activity(&json!({ "hook_event_name": "Stop", "tool_use_id": "t" })).is_none());
    }

    #[test]
    fn truncate_keeps_short_text_and_cuts_long_text_on_a_char_boundary() {
        assert_eq!(truncate("short", 10), "short");
        let cut = truncate("aé", 2);
        assert!(cut.starts_with("a\n"), "{cut}");
        assert!(cut.ends_with("(2 more bytes)"), "{cut}");
    }
}
