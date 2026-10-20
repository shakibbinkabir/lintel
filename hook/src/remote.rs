//! `--remote <host>`: the app runs this relay itself for an event that came in
//! over an SSH host's tunnel (src-tauri/src/remote.rs), so the host's events go
//! through the very same steps and replies as local ones. A host is trusted less
//! than this machine: whatever `lintel_` fields its payload carries are dropped,
//! the event is tagged with the host the app named, and the plan gauge (the
//! status line) stays this machine's. (After upstream pull request 510.)

use serde_json::Value;

/// The host the app named on the command line, after `--remote`.
pub fn host() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--remote" {
            return args.next().filter(|h| !h.is_empty());
        }
    }
    None
}

/// The payload as this machine takes it from `host`. None: drop it, say nothing.
/// Something that is not JSON is passed on as it is and fails like any other.
pub fn tag(raw: &[u8], host: &str) -> Option<Vec<u8>> {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(raw) else { return Some(raw.to_vec()) };
    if map.get("hook_event_name").and_then(Value::as_str) == Some("StatusLine") {
        return None;
    }
    map.retain(|key, _| !key.starts_with("lintel_"));
    map.insert("lintel_remote".into(), Value::String(host.to_string()));
    Some(Value::Object(map).to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hosts_event_keeps_only_what_a_host_may_say() {
        let raw = br#"{"hook_event_name":"PreToolUse","lintel_agent":"codex","lintel_remote":"spoofed","lintel_ancestors":[4]}"#;
        let v: Value = serde_json::from_slice(&tag(raw, "devbox").unwrap()).unwrap();
        assert_eq!(v["lintel_remote"], "devbox");
        assert!(v.get("lintel_agent").is_none() && v.get("lintel_ancestors").is_none());
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert!(tag(br#"{"hook_event_name":"StatusLine"}"#, "devbox").is_none());
        assert_eq!(tag(b"not json", "devbox").unwrap(), b"not json");
    }
}
