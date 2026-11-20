//! Read-only adapter for an already-running local Magpie gateway.
//! Never starts Magpie, opens credential stores, or invokes mutating APIs.
use crate::usage::{Provider, Window, parse_time};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LIMIT: usize = 1024 * 1024;
const TTL: i64 = 60;
/// Magpie refreshes quotas on its own cadence (minutes); only flag data that
/// is clearly older than that, not every card past the 60 s fetch TTL.
const STALE_AFTER: i64 = 600;
const DEFAULT_URL: &str = "http://127.0.0.1:3425";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Magpie,
    Native,
}
#[derive(Clone, Debug)]
pub struct Config {
    pub usage_source: Source,
    pub magpie_url: String,
}
impl Config {
    /// Pure validation; errors never include the untrusted setting value.
    pub fn parse(value: &Value) -> Result<Self, String> {
        if !value.is_object() {
            return Err("Magpie settings must be an object".into());
        }
        let usage_source = match value.get("usage_source") {
            None => Source::Magpie,
            Some(Value::String(s)) if s == "magpie" => Source::Magpie,
            Some(Value::String(s)) if s == "native" => Source::Native,
            _ => return Err("Invalid usage_source: choose magpie or native".into()),
        };
        // A dormant gateway preference must not block explicitly selected native mode.
        if usage_source == Source::Native {
            return Ok(Self {
                usage_source,
                magpie_url: DEFAULT_URL.into(),
            });
        }
        let url = match value.get("magpie_url") {
            None => DEFAULT_URL,
            Some(Value::String(s)) => s,
            _ => return Err("Invalid Magpie URL: expected HTTP loopback endpoint".into()),
        };
        let magpie_url =
            endpoint(url).ok_or("Invalid Magpie URL: expected HTTP loopback endpoint")?;
        Ok(Self {
            usage_source,
            magpie_url,
        })
    }
}

fn endpoint(url: &str) -> Option<String> {
    let authority = url
        .strip_prefix("http://")?
        .strip_suffix('/')
        .unwrap_or(url.strip_prefix("http://")?);
    let (host, port) = if let Some(rest) = authority.strip_prefix("[::1]") {
        ("[::1]", rest.strip_prefix(':'))
    } else if let Some((h, p)) = authority.split_once(':') {
        (h, Some(p))
    } else {
        (authority, None)
    };
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    if let Some(port) = port {
        if port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port.parse::<u16>().ok()? == 0
        {
            return None;
        }
    } else if host == "[::1]" && authority != "[::1]" {
        return None;
    }
    Some(format!("http://{authority}"))
}

pub fn load_config() -> Result<Config, String> {
    let path = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("herdr-sidebar/settings.json"));
    let Some(path) = path else {
        return Config::parse(&serde_json::json!({}));
    };
    let bytes = match read_bounded(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Config::parse(&serde_json::json!({}));
        }
        Err(_) => return Err("Cannot read Magpie settings".into()),
    };
    let value = serde_json::from_slice(&bytes).map_err(|_| "Invalid Magpie settings JSON")?;
    Config::parse(&value)
}

pub fn unavailable(error: &str) -> Provider {
    Provider {
        name: "Magpie",
        plan: None,
        account: None,
        windows: vec![],
        error: Some(error.into()),
        note: None,
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
fn clean(s: &str) -> String {
    // Terminal controls and bidi overrides cannot cross the adapter boundary.
    let text: String = s
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(160)
        .collect();
    if ["Bearer ", "sk-", "access_token", "refresh_token", "eyJ"]
        .iter()
        .any(|s| text.contains(s))
    {
        "[redacted]".into()
    } else {
        text
    }
}
fn string(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(clean).filter(|s| !s.is_empty())
}
fn provider_name(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" | "claude-code" => "Claude",
        "codex" | "openai" => "Codex",
        "opencode" | "opencode-go" | "opencode go" => "OpenCode Go",
        "copilot" | "github-copilot" | "github copilot" => "Copilot",
        "gemini" | "gemini-cli" | "gemini cli" | "antigravity" => "Gemini",
        "" | "magpie" => "Magpie",
        _ => "Other",
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Quota {
    label: String,
    used: f64,
    resets_at: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Card {
    name: String,
    plan: Option<String>,
    account: Option<String>,
    windows: Vec<Quota>,
    note: String,
    read_at: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Snapshot {
    endpoint: String,
    attempted: i64,
    failed: bool,
    cards: Vec<Card>,
}

fn parse_cards(body: &[u8]) -> Result<Vec<Card>, String> {
    if body.len() > LIMIT {
        return Err("Magpie response exceeds size limit".into());
    }
    let root: Value = serde_json::from_slice(body).map_err(|_| "Invalid Magpie response JSON")?;
    let cards = root
        .get("data")
        .or_else(|| root.get("cards"))
        .unwrap_or(&root)
        .as_array()
        .ok_or("Magpie response has no cards array")?;
    if cards.len() > 256 {
        return Err("Magpie response has too many cards".into());
    }
    let mut result = Vec::new();
    for v in cards {
        if !v.is_object() {
            return Err("Invalid Magpie card".into());
        }
        // `provider` is Magpie's stable id; `name` is its display label.
        let name = match provider_name(v["provider"].as_str().unwrap_or("")) {
            id @ ("Other" | "Magpie") => match provider_name(v["name"].as_str().unwrap_or("")) {
                "Other" | "Magpie" => id,
                known => known,
            },
            known => known,
        };
        let user = string(v, "user");
        let alias = string(v, "alias");
        let account = match (alias, user) {
            (Some(a), Some(u)) => Some(format!("{a} · {u}")),
            (a, u) => a.or(u),
        };
        let mut windows = Vec::new();
        let mut notes = Vec::new();
        if v.get("error")
            .is_some_and(|e| !e.is_null() && e != &Value::Bool(false))
            || v["failed"].as_bool() == Some(true)
        {
            notes.push("provider quota unavailable".into());
        }
        if let Some(items) = v["windows"].as_array() {
            for w in items.iter().take(64) {
                let mut label = string(w, "name").unwrap_or_else(|| "Quota".into());
                let display = string(w, "display");
                // Unmetered windows are a fact, not 0% usage.
                if display
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case("unlimited"))
                {
                    notes.push(format!("{label}: unlimited"));
                    continue;
                }
                let pct = |v: &Value| {
                    v.as_f64()
                        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
                };
                let text_only = w["text"].as_bool() == Some(true)
                    || w["display"].as_str().is_some_and(|s| {
                        s.eq_ignore_ascii_case("unknown") || s.eq_ignore_ascii_case("text")
                    });
                let used = if text_only {
                    None
                } else {
                    pct(&w["used"]).or_else(|| {
                        // Invalid explicit usage is not repaired by another field.
                        if w.get("used").is_some_and(|v| !v.is_null()) {
                            None
                        } else {
                            pct(&w["remaining"]).map(|r| 100.0 - r)
                        }
                    })
                };
                if let Some(used) = used {
                    // Keep Magpie's own count (e.g. "0 / 200") beside the percentage.
                    if let Some(d) = display
                        .as_deref()
                        .filter(|d| d.chars().any(|c| c.is_ascii_digit()) && !d.ends_with('%'))
                    {
                        label = format!("{label} {}", d.replace(" / ", "/"));
                    }
                    windows.push(Quota {
                        label,
                        used,
                        resets_at: w["resetsAt"].as_str().and_then(parse_time),
                    });
                } else {
                    notes.push(format!("{label}: Unknown"));
                }
            }
        }
        // Only explicitly whitelisted human notices survive; no provider/error bodies.
        for obj in [v, &root] {
            if let Some(notice) = string(obj, "notice") {
                notes.push(notice);
            }
            if let Some(notices) = obj["notices"].as_array() {
                notes.extend(notices.iter().filter_map(Value::as_str).take(8).map(clean));
            }
        }
        if windows.is_empty() && notes.is_empty() {
            notes.push("Quota: Unknown".into());
        }
        let read_at = string(v, "readAt").or_else(|| string(&root, "readAt"));
        result.push(Card {
            name: name.into(),
            plan: string(v, "plan"),
            account,
            windows,
            note: notes.join(" · ").chars().take(512).collect(),
            read_at,
        });
    }
    result.sort_by_key(|card| match card.name.as_str() {
        "Claude" => 0,
        "Codex" => 1,
        "Copilot" => 2,
        "OpenCode Go" => 3,
        "Gemini" => 4,
        _ => 5,
    });
    Ok(result)
}

fn render(snapshot: &Snapshot, time: i64, status: Option<&str>) -> Vec<Provider> {
    let mut result = Vec::new();
    for card in &snapshot.cards {
        let source_time = card.read_at.as_deref().and_then(parse_time);
        // A busy lock ("updating") is not staleness; only failure or old data is.
        let stale = snapshot.failed
            || status == Some("cache unavailable")
            || source_time.is_none_or(|t| time.saturating_sub(t) >= STALE_AFTER);
        let mut notes = Vec::new();
        if !card.note.is_empty() {
            notes.push(card.note.clone());
        }
        notes.push(format!(
            "Magpie readAt: {}",
            card.read_at.as_deref().unwrap_or("Unknown")
        ));
        if stale {
            notes.push("stale".into());
        }
        if let Some(s) = status {
            notes.push(s.into());
        }
        result.push(Provider {
            name: provider_name(&card.name),
            plan: card.plan.clone(),
            account: card.account.clone(),
            windows: card
                .windows
                .iter()
                .filter(|w| w.used.is_finite() && (0.0..=100.0).contains(&w.used))
                .map(|w| Window {
                    label: w.label.clone(),
                    used: w.used,
                    resets_at: w.resets_at,
                    length_secs: None,
                    limited: w.used >= 100.0,
                })
                .collect(),
            error: snapshot
                .failed
                .then(|| "Magpie refresh failed; showing last good quotas".into()),
            note: Some(notes.join(" · ")),
        });
    }
    if result.is_empty() {
        result.push(unavailable(if snapshot.failed {
            "Magpie unavailable; retrying after backoff"
        } else {
            "Magpie returned no quota cards"
        }));
    }
    result
}

fn private_file(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}
fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err(std::io::Error::other("size limit"));
    }
    Ok(bytes)
}
fn unique_path(dir: &Path, label: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    dir.join(format!(
        "{label}-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}
fn create_unique(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}
fn save(dir: &Path, path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
    let tmp = unique_path(dir, "snapshot");
    let result = (|| {
        let mut file = create_unique(&tmp)?;
        serde_json::to_writer(&mut file, snapshot)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

/// File-backed output avoids unbounded pipe readers and inherited-pipe hangs.
/// curl has its own six-second deadline; the parent also kills/reaps at seven.
fn request(url: &str, dir: &Path) -> Result<Vec<Card>, String> {
    let tmp = unique_path(dir, "response");
    let result = (|| {
        let output = create_unique(&tmp).map_err(|_| "Magpie response storage unavailable")?;
        let mut command = Command::new("curl");
        command
            .args([
                "--disable",
                "--silent",
                "--fail",
                "--noproxy",
                "*",
                "--proxy",
                "",
                "--proto",
                "=http",
                "--max-time",
                "6",
                "--connect-timeout",
                "2",
                "--max-filesize",
                "1048576",
                "--url",
                &format!("{url}/v1/magpie/quotas"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .map_err(|_| "Cannot start curl for Magpie")?;
        let deadline = Instant::now() + Duration::from_secs(7);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return Err("Magpie gateway request failed".into());
                    }
                    break;
                }
                Ok(None)
                    if Instant::now() < deadline
                        && std::fs::metadata(&tmp).is_ok_and(|m| m.len() <= LIMIT as u64) =>
                {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("Magpie request timed out or exceeded size limit".into());
                }
            }
        }
        parse_cards(&read_bounded(&tmp).map_err(|_| "Magpie response unavailable or oversized")?)
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

pub fn fetch(config: &Config) -> Vec<Provider> {
    fetch_in(config, &crate::rundir::dir("magpie"), now(), request)
}
fn fetch_in(
    config: &Config,
    dir: &Path,
    time: i64,
    get: impl FnOnce(&str, &Path) -> Result<Vec<Card>, String>,
) -> Vec<Provider> {
    if endpoint(&config.magpie_url).as_deref() != Some(config.magpie_url.as_str()) {
        return vec![unavailable("Invalid Magpie endpoint")];
    }
    if crate::rundir::ensure_private(dir).is_err() {
        return vec![unavailable("Magpie private cache unavailable")];
    }
    // No hash collisions: the strict authority alphabet has an injective filename encoding.
    let key: String = config
        .magpie_url
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    let cache = dir.join(format!("{key}.json"));
    let load = || {
        read_bounded(&cache)
            .ok()
            .and_then(|b| serde_json::from_slice::<Snapshot>(&b).ok())
            .filter(|s| s.endpoint == config.magpie_url)
    };
    let Some(lock) = private_file(&dir.join(format!("{key}.lock"))).ok() else {
        return vec![unavailable("Magpie cache lock unavailable")];
    };
    if lock.try_lock().is_err() {
        return load()
            .map(|s| render(&s, time, Some("updating")))
            .unwrap_or_else(|| vec![unavailable("Magpie updating; no cached quotas available")]);
    }
    let mut snapshot = load().unwrap_or(Snapshot {
        endpoint: config.magpie_url.clone(),
        attempted: 0,
        failed: false,
        cards: vec![],
    });
    if snapshot.attempted > 0 && time.saturating_sub(snapshot.attempted) < TTL {
        return render(&snapshot, time, None);
    }
    snapshot.attempted = time;
    match get(&config.magpie_url, dir) {
        Ok(cards) => {
            snapshot.cards = cards;
            snapshot.failed = false;
        }
        Err(_) => snapshot.failed = true,
    }
    let saved = save(dir, &cache, &snapshot).is_ok();
    render(&snapshot, time, (!saved).then_some("cache unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_config() {
        assert_eq!(
            Config::parse(&serde_json::json!({})).unwrap().usage_source,
            Source::Magpie
        );
        assert_eq!(
            Config::parse(&serde_json::json!({"usage_source":"native"}))
                .unwrap()
                .usage_source,
            Source::Native
        );
        for url in [
            "https://localhost",
            "http://remote",
            "http://localhost@remote",
            "http://localhost/path",
            "http://localhost?x",
            "http://[::1]:bad",
            "http://localhost:0",
            "http://127.1",
            "http://localhost\n",
        ] {
            assert!(endpoint(url).is_none(), "{url}");
        }
        for url in [DEFAULT_URL, "http://localhost:3425/", "http://[::1]:3425"] {
            assert!(endpoint(url).is_some(), "{url}");
        }
        assert!(Config::parse(&serde_json::json!({"usage_source":"typo"})).is_err());
        assert_eq!(Config::parse(&serde_json::json!({"usage_source":"native", "magpie_url":"https://dormant.invalid"})).unwrap().usage_source, Source::Native);
    }
    fn fixture() -> Vec<Card> {
        parse_cards(br#"{"readAt":"2026-11-16T08:00:00+08:00","cards":[{"provider":"claude","alias":"Work","user":"same@example.test","plan":"Pro","token":"secret","windows":[{"name":"Weekly","used":42,"resetsAt":"2026-11-17T08:00:00+08:00"},{"name":"Image generation","text":"Unknown"}]},{"provider":"claude","alias":"Personal","user":"same@example.test","windows":[{"name":"Daily","remaining":80}]}]}"#).unwrap()
    }
    #[test]
    fn parser_keeps_context_and_unknown_without_fake_zero() {
        let cards = fixture();
        assert_eq!(cards.len(), 2);
        assert_ne!(cards[0].account, cards[1].account);
        assert_eq!(cards[0].windows.len(), 1);
        assert!(cards[0].note.contains("Image generation: Unknown"));
        assert_eq!(cards[1].windows[0].used, 20.0);
        assert!(!serde_json::to_string(&cards).unwrap().contains("secret"));
        let bad = parse_cards(br#"{"cards":[{"provider":"sk-secret","windows":[{"name":"Bad","used":101},{"name":"Text","used":"50"},{"name":"Negative","used":-1}]}]}"#).unwrap();
        assert!(bad[0].windows.is_empty());
        assert_eq!(bad[0].name, "Other");
        let copilot = parse_cards(br#"{"data":[{"provider":"copilot","name":"Copilot","windows":[{"name":"Premium requests","used":0,"display":"0 / 200"},{"name":"Chat requests","used":0,"display":"Unlimited"}]}]}"#).unwrap();
        assert_eq!(copilot[0].name, "Copilot");
        assert_eq!(copilot[0].windows.len(), 1);
        assert_eq!(copilot[0].windows[0].label, "Premium requests 0/200");
        assert!(copilot[0].note.contains("Chat requests: unlimited"));
    }
    #[test]
    fn cache_reuses_backoffs_retains_good_and_separates_endpoints() {
        let dir =
            std::env::temp_dir().join(format!("magpie-test-{}-{}", std::process::id(), now()));
        let config = Config::parse(&serde_json::json!({})).unwrap();
        let a = fetch_in(&config, &dir, 100, |_, _| Ok(fixture()));
        assert_eq!(a.len(), 2);
        fetch_in(&config, &dir, 110, |_, _| {
            panic!("fresh cache must not fetch")
        });
        let b = fetch_in(&config, &dir, 161, |_, _| Err("secret raw failure".into()));
        assert!(b[0].error.is_some());
        assert_eq!(a[0].windows, b[0].windows);
        assert!(b[0].note.as_ref().unwrap().contains("stale"));
        fetch_in(&config, &dir, 170, |_, _| panic!("failure must back off"));
        let other =
            Config::parse(&serde_json::json!({"magpie_url":"http://localhost:9999"})).unwrap();
        let c = fetch_in(&other, &dir, 170, |_, _| Err("no".into()));
        assert!(c[0].windows.is_empty());
        let key: String = config
            .magpie_url
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        let lock = private_file(&dir.join(format!("{key}.lock"))).unwrap();
        lock.lock().unwrap();
        let d = fetch_in(&config, &dir, 180, |_, _| {
            panic!("busy lock must not fetch")
        });
        assert!(d[0].note.as_ref().unwrap().contains("updating"));
        drop(lock);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn read_at_unknown_is_explicit_and_source_time_is_used() {
        let s = Snapshot {
            endpoint: DEFAULT_URL.into(),
            attempted: 100,
            failed: false,
            cards: fixture(),
        };
        let fresh = render(&s, parse_time("2026-11-16T00:02:00Z").unwrap(), None);
        assert!(!fresh[0].note.as_ref().unwrap().contains("stale"));
        let busy = render(
            &s,
            parse_time("2026-11-16T00:02:00Z").unwrap(),
            Some("updating"),
        );
        assert!(!busy[0].note.as_ref().unwrap().contains("stale"));
        let shown = render(&s, parse_time("2026-11-16T00:20:00Z").unwrap(), None);
        assert!(shown[0].note.as_ref().unwrap().contains("stale"));
        assert!(shown[0].note.as_ref().unwrap().contains("+08:00"));
        assert!(parse_cards(b"{\"cards\":[]}").unwrap().is_empty());
        assert!(parse_cards(b"{}").is_err());
        for body in [br#"{"data":[{"provider":"claude","windows":[{"name":"Image generation","used":0,"remaining":100,"display":"Unknown","text":true}]}],"object":"list"}"#.as_slice(), br#"[{"provider":"claude","windows":[{"name":"Image generation","used":0,"remaining":100,"display":"Unknown","text":true}]}]"#.as_slice()] {
            let cards = parse_cards(body).unwrap();
            assert_eq!(cards.len(), 1);
            assert!(cards[0].windows.is_empty());
            assert_eq!(cards[0].note, "Image generation: Unknown");
        }
        let cards = parse_cards(br#"{"data":[{"error":"secret raw body","windows":[]}]}"#).unwrap();
        assert!(cards[0].note.contains("provider quota unavailable"));
        assert!(!cards[0].note.contains("secret"));
    }
}
