//! What the relay prints back to the agent — the only place that knows each
//! agent's stdout contract. Same replies as the Mac relay (HookServer.swift).
//!
//! The one rule that matters: **nothing that allows anything is ever printed
//! without a decision a human clicked.** With no decision the reply is silence,
//! an empty `{}`, or Copilot's explicit "ask", and every agent then asks in its
//! own terminal exactly as if Lintel were not installed.

use serde_json::{json, Map, Value};

/// Claude Code under another name: `--agent claude-<something>` gives a second
/// Claude profile its own pill and keeps Claude Code's stdout contract (decisions
/// and answered questions included). `claude-desktop` is not one: that tag marks
/// sessions of the Claude desktop app, which has its own prompts and stays silent.
/// (After upstream pull request 607.)
pub fn is_claude_code(agent: &str) -> bool {
    agent.is_empty() || (agent.starts_with("claude-") && agent != "claude-desktop")
}

/// The agents whose permission requests the island can answer: Claude Code
/// (no `--agent`, or a `claude-*` alias), Codex, Copilot CLI, Muse Code, Cursor
/// Agent, Kiro CLI, Antigravity and OpenCode. Any decision for another agent is
/// ignored here too, whatever the app sent.
pub fn takes_decisions(agent: &str) -> bool {
    is_claude_code(agent) || matches!(agent, "codex" | "copilot" | "muse" | "cursor" | "kiro" | "antigravity" | "opencode")
}

/// What goes to stderr, and the exit code, when the answer is not on stdout:
/// Kiro CLI blocks a tool when its PreToolUse hook exits with 2 and tells the
/// model what stderr says. Anything else, a missing decision included, exits 0.
pub fn exit_status(agent: &str, event: &str, decision: Option<&str>) -> Option<(String, i32)> {
    let denied = decision.map(str::trim) == Some("deny");
    (agent == "kiro" && event == "PermissionRequest" && denied).then(|| (format!("Denied from {}", notch_brand::NAME), 2))
}

/// Agents that read a JSON object on stdout after every hook and get `{}` —
/// "no opinion" — when there is nothing to say.
fn wants_json(agent: &str) -> bool {
    matches!(agent, "gemini" | "antigravity" | "copilot" | "muse")
}

/// The line to print for `event` from `agent`, given the island's `decision`
/// (`None`: nobody clicked) and, for Claude Code's AskUserQuestion, the
/// question as it was asked.
pub fn stdout(agent: &str, event: &str, decision: Option<&str>, question: Option<&Value>) -> Option<String> {
    if event != "PermissionRequest" {
        // Antigravity reads "{}" on PreToolUse as a denial. "ask" keeps its own prompt
        // (and the user's Always Allow): Lintel never allows a tool by itself.
        if agent.eq_ignore_ascii_case("antigravity") && event == "PreToolUse" {
            return Some(r#"{"decision":"ask"}"#.to_string());
        }
        return wants_json(agent).then(|| "{}".to_string());
    }
    let decision = decision.filter(|_| takes_decisions(agent));
    match agent {
        // Copilot CLI and Muse Code read a bare permissionDecision. Copilot is
        // fail-closed: it must always get valid JSON, so silence is an "ask".
        "copilot" | "muse" => {
            let word = match decision.map(str::trim) {
                Some("allow" | "always") => Some("allow"),
                Some("deny") => Some("deny"),
                _ => None,
            };
            match word {
                Some(word) => Some(json!({ "permissionDecision": word }).to_string()),
                None if agent == "copilot" => Some(json!({ "permissionDecision": "ask" }).to_string()),
                None => None,
            }
        }
        // Cursor reads a flat `permission`.
        "cursor" => match decision.map(str::trim) {
            Some("allow" | "always") => Some(r#"{"permission":"allow"}"#.to_string()),
            Some("deny") => Some(
                json!({
                    "permission": "deny",
                    "user_message": format!("Denied from {}", notch_brand::NAME),
                    "agent_message": format!("The user denied this command from {}.", notch_brand::NAME),
                })
                .to_string(),
            ),
            _ => None,
        },
        // Kiro reads its answer from the exit code (`exit_status`).
        "kiro" => None,
        // Antigravity gates a tool on PreToolUse: no click is "ask", which leaves it
        // to its own prompt. (The exact grants for an allow are added by `stdout_with`.)
        "antigravity" => match decision.map(str::trim) {
            Some("allow" | "always") => Some(r#"{"decision":"allow"}"#.to_string()),
            Some("deny") => Some(json!({ "decision": "deny", "reason": format!("Denied from {}", notch_brand::NAME) }).to_string()),
            _ => Some(r#"{"decision":"ask"}"#.to_string()),
        },
        // Claude Code (under any of its names), Codex and OpenCode share the
        // documented hookSpecificOutput. Claude Code and OpenCode ask questions.
        a if is_claude_code(a) || a == "opencode" => decision.and_then(|d| decision_json(d, question)),
        _ => decision.and_then(|d| decision_json(d, None)),
    }
}

/// `stdout`, plus Claude Code's "Always": the decision "always" echoes the
/// request's own `permission_suggestions` as `updatedPermissions`, so Claude Code
/// remembers the rule. No suggestion (or another agent) means a plain allow.
/// For Antigravity `suggestions` is the tool call (`{"name", "args"}`) instead,
/// and an allow carries the exact grant for that one command or file.
pub fn stdout_with(
    agent: &str,
    event: &str,
    decision: Option<&str>,
    question: Option<&Value>,
    suggestions: Option<&Value>,
) -> Option<String> {
    if agent == "antigravity" && event == "PermissionRequest" && matches!(decision.map(str::trim), Some("allow" | "always")) {
        let grants = antigravity_grants(suggestions);
        if !grants.is_empty() {
            return Some(json!({ "decision": "allow", "permissionOverrides": grants }).to_string());
        }
    }
    if is_claude_code(agent) && event == "PermissionRequest" && decision == Some("always") {
        if let Some(list) = suggestions.and_then(Value::as_array).filter(|l| !l.is_empty()) {
            return Some(
                json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PermissionRequest",
                        "decision": { "behavior": "allow", "updatedPermissions": list },
                    }
                })
                .to_string(),
            );
        }
    }
    stdout(agent, event, decision, question)
}

/// The exact grants for one Antigravity tool call, in the forms its permission
/// rules read: the command line as typed, or the one file being written. No
/// wildcard and no binary name: allowing `git status` once never allows `git`.
fn antigravity_grants(call: Option<&Value>) -> Vec<String> {
    let name = call.and_then(|c| c.get("name")).and_then(Value::as_str).unwrap_or_default();
    let args = call.and_then(|c| c.get("args")).and_then(Value::as_object);
    let text = |keys: &[&str]| keys.iter().find_map(|k| args?.get(*k)?.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let mut list = Vec::new();
    if name == "run_command" {
        if let Some(cmd) = text(&["CommandLine", "command"]) {
            list.push(format!("command({cmd})"));
            list.push(format!("run_command({cmd})"));
        }
    } else if matches!(name, "write_to_file" | "replace_file_content" | "multi_replace_file_content") {
        if let Some(file) = text(&["TargetFile", "file_path"]) {
            list.push(format!("{name}({file})"));
        }
    }
    list
}

/// The documented PermissionRequest output. Anything we do not recognise prints
/// nothing at all rather than guessing — silence is the safe answer.
/// See https://code.claude.com/docs/en/hooks
///
/// `question` is the AskUserQuestion input, when that is what is being asked:
/// the island may then answer it, which Claude Code takes as the same input
/// with an `answers` map added. Nothing else of the input can be changed from
/// the island, and no other tool's input can be changed at all.
pub fn decision_json(decision: &str, question: Option<&Value>) -> Option<String> {
    if decision.trim_start().starts_with('{') {
        let reply = serde_json::from_str::<Value>(decision).ok()?;
        let answers = reply.get("answers")?.as_object()?;
        // The questions answered in words under "Other…" (upstream pull request
        // 335). It only says how to check the answers: it never reaches the agent.
        let other: Vec<&str> = match reply.get("other") {
            None => Vec::new(),
            Some(list) => list.as_array()?.iter().map(Value::as_str).collect::<Option<_>>()?,
        };
        let question = question?;
        if !answers_fit(question, answers, &other) {
            return None;
        }
        let mut input = question.as_object()?.clone();
        input.insert("answers".into(), Value::Object(answers.clone()));
        return Some(
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": { "behavior": "allow", "updatedInput": input },
                }
            })
            .to_string(),
        );
    }
    let behavior = match decision.trim() {
        // "always" still answers a plain allow; remembering it is the island's
        // business, not the agent's.
        "allow" | "always" => r#"{"behavior":"allow"}"#.to_string(),
        "deny" => format!(r#"{{"behavior":"deny","message":"Denied from {}"}}"#, notch_brand::NAME),
        _ => return None,
    };
    Some(format!(
        r#"{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{behavior}}}}}"#
    ))
}

/// Claude Code's ExitPlanMode, answered on the plan card (after upstream pull
/// request 703). Claude Code ignores an allow for it that does not carry the
/// plan back as `updatedInput`. Auto mode and Manual are the terminal's own
/// "yes, and…" choices, a `setMode` for the session; Change plan is its "tell
/// Claude what to change": a deny that stops the turn, so the user types the
/// change in the terminal. Anything else prints nothing.
pub fn plan_json(decision: &str, plan: &Value) -> Option<String> {
    let mut d = match decision.trim() {
        "allow" | "always" | "plan_auto" | "plan_manual" => json!({ "behavior": "allow", "updatedInput": plan }),
        "plan_change" => json!({
            "behavior": "deny",
            "interrupt": true,
            "message": "The user wants to change the plan and will say how in the terminal.",
        }),
        "deny" => return decision_json("deny", None),
        _ => return None,
    };
    let mode = match decision.trim() {
        "plan_auto" => "auto",
        "plan_manual" => "default",
        _ => "",
    };
    if !mode.is_empty() {
        d["updatedPermissions"] = json!([{ "type": "setMode", "mode": mode, "destination": "session" }]);
    }
    Some(json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": d } }).to_string())
}

/// True when `answers` answers exactly the questions Claude Code asked: one entry
/// per question, keyed by its text; a single-select answer is one of its option
/// labels (a string), a multi-select answer a non-empty list of distinct labels
/// (Claude Code 2.1.136+ takes the list, as the Mac sends it). Same rule as
/// `QuestionPayload.accepts` on macOS, plus the questions listed in `other`,
/// answered in words under "Other…" as in the terminal: any text that is not
/// blank, and nothing but text.
fn answers_fit(question: &Value, answers: &Map<String, Value>, other: &[&str]) -> bool {
    let Some(items) = question.get("questions").and_then(|q| q.as_array()) else { return false };
    if items.is_empty() || items.len() != answers.len() {
        return false;
    }
    items.iter().all(|item| {
        let Some(text) = item.get("question").and_then(|q| q.as_str()) else { return false };
        if other.contains(&text) {
            return matches!(answers.get(text), Some(Value::String(words)) if !words.trim().is_empty());
        }
        let labels: Vec<&str> = item
            .get("options")
            .and_then(|o| o.as_array())
            .map(|opts| opts.iter().filter_map(|o| o.get("label").and_then(|l| l.as_str())).collect())
            .unwrap_or_default();
        let multi = item.get("multiSelect").and_then(|m| m.as_bool()).unwrap_or(false);
        match answers.get(text) {
            Some(Value::String(pick)) if !multi => labels.contains(&pick.as_str()),
            Some(Value::Array(picks)) if multi => {
                let picks: Vec<&str> = picks.iter().filter_map(|p| p.as_str()).collect();
                let mut seen = picks.clone();
                seen.sort_unstable();
                seen.dedup();
                !picks.is_empty() && seen.len() == picks.len() && picks.iter().all(|p| labels.contains(p))
            }
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: &[&str] = &[
        "", "gemini", "antigravity", "cursor", "codex", "copilot", "muse", "opencode", "amp",
        "hermes", "claude-desktop", "kiro", "claude-perso", "my-tool",
    ];
    const EVENTS: &[&str] = &[
        "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure",
        "PermissionRequest", "Notification", "Stop", "StopFailure", "SessionEnd", "Interrupt",
        "SubagentStart", "SubagentStop", "",
    ];

    /// Whatever a reply says, does it let anything through?
    fn allows(out: &str) -> bool {
        let v: Value = serde_json::from_str(out).expect("every reply is JSON");
        let text = v.to_string();
        // Any decision counts as one, except "ask" (Antigravity's PreToolUse): that hands the
        // choice back to the agent's own prompt.
        let decided = v.get("decision").map_or(false, |d| d.as_str() != Some("ask"));
        text.contains("allow") || decided || v.get("permission").is_some() || v.get("continue").is_some()
    }

    #[test]
    fn nothing_is_ever_allowed_without_a_decision() {
        for agent in AGENTS {
            for event in EVENTS {
                if let Some(out) = stdout(agent, event, None, None) {
                    assert!(!allows(&out), "{agent:?} {event} printed {out} with nobody clicking");
                }
                // A decline, a timeout or garbage from the app is no decision either.
                for not_a_decision in ["", "ask", "maybe", "{\"permissionDecision\":\"allow\"}"] {
                    if let Some(out) = stdout(agent, event, Some(not_a_decision), None) {
                        assert!(!allows(&out), "{agent:?} {event} {not_a_decision:?} printed {out}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_decision_only_counts_on_a_permission_request_from_an_agent_that_takes_one() {
        for agent in AGENTS {
            for event in EVENTS.iter().filter(|e| **e != "PermissionRequest") {
                if let Some(out) = stdout(agent, event, Some("allow"), None) {
                    assert!(!allows(&out), "{agent:?} {event}: {out}");
                }
            }
            let out = stdout(agent, "PermissionRequest", Some("allow"), None);
            // Kiro answers through its exit code, never on stdout.
            let on_stdout = takes_decisions(agent) && *agent != "kiro";
            assert_eq!(out.as_deref().is_some_and(allows), on_stdout, "{agent:?}: {out:?}");
        }
    }

    #[test]
    fn each_agent_gets_its_own_reply_shape() {
        let allow = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
        let deny = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied from Lintel"}}}"#;
        for agent in ["", "codex"] {
            assert_eq!(stdout(agent, "PermissionRequest", Some("allow"), None).unwrap(), allow);
            assert_eq!(stdout(agent, "PermissionRequest", Some("always"), None).unwrap(), allow);
            assert_eq!(stdout(agent, "PermissionRequest", Some("deny"), None).unwrap(), deny);
            assert_eq!(stdout(agent, "PermissionRequest", None, None), None);
            assert_eq!(stdout(agent, "PreToolUse", None, None), None);
        }
        for agent in ["copilot", "muse"] {
            assert_eq!(stdout(agent, "PermissionRequest", Some("allow"), None).unwrap(), r#"{"permissionDecision":"allow"}"#);
            assert_eq!(stdout(agent, "PermissionRequest", Some("deny"), None).unwrap(), r#"{"permissionDecision":"deny"}"#);
            assert_eq!(stdout(agent, "PreToolUse", None, None).unwrap(), "{}");
        }
        // Copilot is fail-closed: no decision is an explicit "ask", never silence.
        assert_eq!(stdout("copilot", "PermissionRequest", None, None).unwrap(), r#"{"permissionDecision":"ask"}"#);
        assert_eq!(stdout("muse", "PermissionRequest", None, None), None);
        // Antigravity needs a decision on PreToolUse: "ask" leaves it to Antigravity's own prompt;
        // other lifecycle events receive "{}".
        assert_eq!(stdout("antigravity", "PreToolUse", None, None).unwrap(), r#"{"decision":"ask"}"#);
        for event in ["PostToolUse", "UserPromptSubmit", "Stop"] {
            assert_eq!(stdout("antigravity", event, None, None).unwrap(), "{}");
        }
        // Gemini CLI reads "{}" as "no opinion".
        for event in ["PreToolUse", "PostToolUse", "UserPromptSubmit", "Stop"] {
            assert_eq!(stdout("gemini", event, None, None).unwrap(), "{}");
        }
        // Cursor: silence, which Cursor reads as "carry on as usual", and a flat
        // `permission` once a shell command was answered.
        for event in ["PreToolUse", "UserPromptSubmit", "Stop"] {
            assert_eq!(stdout("cursor", event, None, None), None);
        }
        assert_eq!(stdout("cursor", "PermissionRequest", Some("allow"), None).unwrap(), r#"{"permission":"allow"}"#);
        assert!(stdout("cursor", "PermissionRequest", Some("deny"), None).unwrap().contains(r#""permission":"deny""#));
        assert_eq!(stdout("cursor", "PermissionRequest", None, None), None);
        // Antigravity: no click is "ask", its own prompt.
        assert_eq!(stdout("antigravity", "PermissionRequest", None, None).unwrap(), r#"{"decision":"ask"}"#);
        assert_eq!(stdout("antigravity", "PermissionRequest", Some("deny"), None).unwrap(), r#"{"decision":"deny","reason":"Denied from Lintel"}"#);
        // OpenCode is not fail-closed: silence, and its plugin asks in its own prompt.
        assert_eq!(stdout("opencode", "PermissionRequest", None, None), None);
        assert_eq!(stdout("opencode", "PermissionRequest", Some("allow"), None).unwrap(), allow);
        // Hermes approvals are not supported: its decisions are never relayed.
        assert_eq!(stdout("hermes", "PermissionRequest", Some("allow"), None), None);
    }

    #[test]
    fn kiro_is_only_ever_blocked_by_a_deny() {
        assert_eq!(exit_status("kiro", "PermissionRequest", Some("deny")), Some(("Denied from Lintel".into(), 2)));
        for decision in [None, Some("allow"), Some("always"), Some(""), Some("maybe")] {
            assert_eq!(exit_status("kiro", "PermissionRequest", decision), None, "{decision:?}");
        }
        assert_eq!(exit_status("kiro", "PreToolUse", Some("deny")), None);
        for agent in AGENTS.iter().filter(|a| **a != "kiro") {
            assert_eq!(exit_status(agent, "PermissionRequest", Some("deny")), None, "{agent:?}");
        }
    }

    #[test]
    fn antigravity_is_allowed_for_that_one_command_only() {
        let call = json!({ "name": "run_command", "args": { "CommandLine": "git status" } });
        let out = stdout_with("antigravity", "PermissionRequest", Some("allow"), None, Some(&call)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["decision"], "allow");
        assert_eq!(v["permissionOverrides"], json!(["command(git status)", "run_command(git status)"]));
        let write = json!({ "name": "write_to_file", "args": { "TargetFile": "C:/p/a.rs" } });
        let out = stdout_with("antigravity", "PermissionRequest", Some("allow"), None, Some(&write)).unwrap();
        assert!(out.contains("write_to_file(C:/p/a.rs)") && !out.contains('*'), "{out}");
        // No click: no grant.
        let out = stdout_with("antigravity", "PermissionRequest", None, None, Some(&call)).unwrap();
        assert_eq!(out, r#"{"decision":"ask"}"#);
    }

    #[test]
    fn a_claude_alias_is_claude_code_with_its_own_pill() {
        assert!(is_claude_code("") && is_claude_code("claude-perso"));
        assert!(!is_claude_code("claude-desktop") && !is_claude_code("codex"));
        assert!(takes_decisions("claude-perso") && !takes_decisions("claude-desktop"));
        let allow = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
        assert_eq!(stdout("claude-perso", "PermissionRequest", Some("allow"), None).unwrap(), allow);
        assert_eq!(stdout("claude-perso", "PermissionRequest", None, None), None);
        assert_eq!(stdout("claude-desktop", "PermissionRequest", Some("allow"), None), None);
    }

    #[test]
    fn decision_json_matches_the_documented_shape() {
        assert_eq!(
            decision_json("allow", None).unwrap(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        );
        assert!(decision_json("always", None).unwrap().contains(r#""behavior":"allow""#));
    }

    #[test]
    fn anything_unrecognised_prints_nothing() {
        assert!(decision_json("", None).is_none());
        assert!(decision_json("maybe", None).is_none());
        // The shape the app used to send must not be mistaken for a decision.
        assert!(decision_json(r#"{"permissionDecision":"allow"}"#, None).is_none());
    }

    #[test]
    fn an_answered_question_goes_back_as_the_same_input_plus_answers() {
        let question = json!({
            "questions": [{ "question": "Which one?", "options": [{ "label": "A" }, { "label": "B" }] }]
        });
        let out = stdout("", "PermissionRequest", Some(r#"{"answers":{"Which one?":"B"}}"#), Some(&question)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let decision = &v["hookSpecificOutput"]["decision"];
        assert_eq!(decision["behavior"], "allow");
        assert_eq!(decision["updatedInput"]["questions"], question["questions"]);
        assert_eq!(decision["updatedInput"]["answers"]["Which one?"], "B");
        // Only Claude Code asks questions: an answer for anyone else is nothing.
        assert!(stdout("codex", "PermissionRequest", Some(r#"{"answers":{"Which one?":"B"}}"#), Some(&question)).is_none());
    }

    #[test]
    fn a_plan_goes_back_with_its_allow_and_only_on_a_click() {
        let plan = json!({ "plan": "# Fix it\n1. Do it" });
        let decision = |word: &str| -> Value {
            serde_json::from_str::<Value>(&plan_json(word, &plan).unwrap()).unwrap()["hookSpecificOutput"]["decision"].clone()
        };
        for word in ["allow", "always", "plan_auto", "plan_manual"] {
            assert_eq!(decision(word)["behavior"], "allow", "{word}");
            assert_eq!(decision(word)["updatedInput"], plan, "{word}");
        }
        assert!(decision("allow").get("updatedPermissions").is_none());
        assert_eq!(decision("plan_auto")["updatedPermissions"][0]["mode"], "auto");
        assert_eq!(decision("plan_manual")["updatedPermissions"][0]["mode"], "default");
        assert_eq!(decision("plan_change")["behavior"], "deny");
        assert_eq!(decision("plan_change")["interrupt"], true);
        assert_eq!(decision("deny")["behavior"], "deny");
        for not_a_decision in ["", "ask", "maybe", r#"{"answers":{}}"#] {
            assert!(plan_json(not_a_decision, &plan).is_none(), "{not_a_decision:?}");
        }
        // A plan word for any other request is nothing at all.
        assert!(stdout("", "PermissionRequest", Some("plan_auto"), None).is_none());
        assert_eq!(stdout("copilot", "PermissionRequest", Some("plan_auto"), None).unwrap(), r#"{"permissionDecision":"ask"}"#);
    }

    #[test]
    fn answers_are_only_accepted_for_a_question() {
        assert!(decision_json(r#"{"answers":{"q":"a"}}"#, None).is_none());
        let question = json!({ "questions": [] });
        assert!(decision_json(r#"{"answers":{}}"#, Some(&question)).is_none());
        assert!(decision_json(r#"{"answers":{"q":1}}"#, Some(&question)).is_none());
        assert!(decision_json(r#"{"command":"rm -rf /"}"#, Some(&question)).is_none());
    }

    #[test]
    fn answers_must_match_the_questions_asked() {
        let q = json!({ "questions": [
            { "question": "Which one?", "options": [{ "label": "A" }, { "label": "B" }] },
            { "question": "Extras?", "multiSelect": true,
              "options": [{ "label": "Tests" }, { "label": "Docs" }, { "label": "Lint" }] }
        ]});
        let ok = |a: &str| decision_json(a, Some(&q)).is_some();
        assert!(ok(r#"{"answers":{"Which one?":"A","Extras?":["Tests","Docs"]}}"#));
        assert!(!ok(r#"{"answers":{"Which one?":"C","Extras?":["Tests"]}}"#));
        assert!(!ok(r#"{"answers":{"Which one?":"A","Extras?":["Tests"],"Other?":"x"}}"#));
        assert!(!ok(r#"{"answers":{"Which one?":"A"}}"#));
        assert!(!ok(r#"{"answers":{"Which one?":["A"],"Extras?":["Tests"]}}"#));
        assert!(!ok(r#"{"answers":{"Which one?":"A","Extras?":"Tests"}}"#));
        assert!(!ok(r#"{"answers":{"Which one?":"A","Extras?":[]}}"#));
        assert!(!ok(r#"{"answers":{"Which one?":"A","Extras?":["Tests","Tests"]}}"#));
    }

    #[test]
    fn words_typed_under_other_are_taken_only_where_marked() {
        let q = json!({ "questions": [
            { "question": "Which one?", "options": [{ "label": "A" }, { "label": "B" }] },
            { "question": "Extras?", "multiSelect": true,
              "options": [{ "label": "Tests" }, { "label": "Docs" }] }
        ]});
        let ok = |a: &str| decision_json(a, Some(&q)).is_some();
        assert!(ok(r#"{"answers":{"Which one?":"Neither, \"C\"\nplease","Extras?":["Docs"]},"other":["Which one?"]}"#));
        assert!(ok(r#"{"answers":{"Which one?":"A","Extras?":"Benchmarks"},"other":["Extras?"]}"#));
        // Not marked: still has to be a label.
        assert!(!ok(r#"{"answers":{"Which one?":"C","Extras?":["Docs"]},"other":[]}"#));
        // Marked: has to be words, not blank and not a list.
        assert!(!ok(r#"{"answers":{"Which one?":"  ","Extras?":["Docs"]},"other":["Which one?"]}"#));
        assert!(!ok(r#"{"answers":{"Which one?":["A"],"Extras?":["Docs"]},"other":["Which one?"]}"#));
        assert!(!ok(r#"{"answers":{"Which one?":"A","Extras?":["Docs"]},"other":"Which one?"}"#));
        assert!(!ok(r#"{"answers":{"Which one?":"A","Extras?":["Docs"]},"other":[1]}"#));
        // The marker never reaches Claude Code: only the answers do.
        let out = decision_json(r#"{"answers":{"Which one?":"C","Extras?":["Docs"]},"other":["Which one?"]}"#, Some(&q)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Which one?"], "C");
        assert!(v["hookSpecificOutput"]["decision"]["updatedInput"].get("other").is_none());
        // Words are an answer to a question, never a permission: any other tool's
        // request still only takes the two words.
        assert!(decision_json(r#"{"answers":{"x":"y"},"other":["x"]}"#, None).is_none());
    }
}
