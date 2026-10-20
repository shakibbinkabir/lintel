// API keys live in the Windows Credential Manager — never on disk and never in
// the front end: a webview can only ask whether a key is present.

use std::sync::RwLock;

use keyring::Entry;

const SERVICE: &str = notch_brand::SLUG;

/// Every key Lintel may store. Anything outside this list is refused.
pub const KNOWN_KEYS: &[&str] = &[
    "anthropic-api-key",
    "anthropic-gateway-token",
    "openai-api-key",
    "google-api-key",
    "openrouter-api-key",
    "openai-compatible-key",
    "n8n-url",
    "n8n-api-key",
    "vercel-token",
    "github-token",
    "stripe-api-key",
    "resend-api-key",
    "notion-api-key",
    "calcom-api-key",
    "linear-api-key",
];

/// The ids of the registered features: each may store keys named `<id>/<name>`.
static FEATURES: RwLock<Vec<&'static str>> = RwLock::new(Vec::new());

/// Called once by the registry at launch.
pub fn allow_features(ids: Vec<&'static str>) {
    *FEATURES.write().unwrap_or_else(|e| e.into_inner()) = ids;
}

/// A core key, or `<feature>/<name>` of a registered feature.
fn known(key: &str) -> bool {
    if KNOWN_KEYS.contains(&key) {
        return true;
    }
    let Some((feature, name)) = key.split_once('/') else { return false };
    let plain = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    plain && FEATURES.read().unwrap_or_else(|e| e.into_inner()).contains(&feature)
}

fn entry(key: &str) -> Option<Entry> {
    if !known(key) {
        return None;
    }
    Entry::new(SERVICE, key).ok()
}

pub fn get(key: &str) -> Option<String> {
    entry(key)?.get_password().ok().filter(|v| !v.is_empty())
}

pub fn set(key: &str, value: &str) -> Result<(), String> {
    let entry = entry(key).ok_or_else(|| format!("unknown key {key}"))?;
    if value.is_empty() {
        let _ = entry.delete_credential();
        return Ok(());
    }
    entry.set_password(value).map_err(|e| e.to_string())
}

pub fn clear(key: &str) -> Result<(), String> {
    let entry = entry(key).ok_or_else(|| format!("unknown key {key}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn present(key: &str) -> bool {
    get(key).is_some()
}
