//! Subscription usage behind the Explorer's Usage view (`4`): how much of
//! each coding-agent plan's rate-limit windows is used, and when each resets.
//! UI-free; `explorer_app.rs` draws it.
//!
//! Sources, each with the credentials its own CLI already keeps (a provider
//! without credentials is simply left out):
//! - Claude: `~/.claude/.credentials.json` → `api.anthropic.com/api/oauth/usage`
//!   (the endpoint Claude Code's `/usage` reads). The answer is cached in
//!   `~/.cache/claude-usage/usage.json`, the same file `claude-usage.sh`
//!   keeps for the prompt, so whichever runs first saves the other a request.
//! - Codex: `~/.codex/auth.json` (else pi's `auth.json`) →
//!   `chatgpt.com/backend-api/wham/usage`.
//! - OpenCode Go: `$OPENCODE_API_KEY`, opencode's or pi's `auth.json` →
//!   `opencode.ai/zen/go/v1/usage`.
//!
//! Tokens go to curl on stdin (`--config -`), never on its command line,
//! where any user on the machine could read them in /proc.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

/// One rate-limit window of a plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub label: String,
    /// 0–100.
    pub used: f64,
    /// Unix seconds.
    pub resets_at: Option<i64>,
    /// Length of the window, when known — lets the view mark an even pace.
    pub length_secs: Option<i64>,
    /// The provider says this window is exhausted.
    pub limited: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub name: &'static str,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    /// Shown instead of (or under) the windows when fetching failed.
    pub error: Option<String>,
    /// A calm, dim remark: e.g. the numbers are a few minutes old.
    pub note: Option<String>,
}

/// Every configured provider, fetched now. Slow (network): run it off the
/// UI thread.
pub fn fetch_all() -> Vec<Provider> {
    [claude(), codex(), opencode_go()]
        .into_iter()
        .flatten()
        .collect()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn read_json(path: PathBuf) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// GET `url` with `headers`, through curl with the headers on stdin.
fn get(url: &str, headers: &[String]) -> Result<Value, String> {
    let mut config = format!("url = \"{url}\"\nsilent\nshow-error\nfail\nmax-time = 10\n");
    for header in headers {
        config.push_str(&format!("header = \"{}\"\n", header.replace('"', "\\\"")));
    }
    let mut child = Command::new("curl")
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("curl: no stdin")?
        .write_all(config.as_bytes())
        .map_err(|e| format!("curl: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim().trim_start_matches("curl: ");
        return Err(if err.contains("401") || err.contains("403") {
            "not signed in (token expired?)".into()
        } else if err.contains("429") {
            "rate limited by the API: showing the last numbers".into()
        } else if err.contains("(28)") || err.contains("(6)") || err.contains("(7)") {
            "offline: showing the last numbers".into()
        } else if err.is_empty() {
            "request failed".into()
        } else {
            err.chars().take(80).collect()
        });
    }
    serde_json::from_slice(&out.stdout).map_err(|_| "unexpected answer".into())
}

/// ISO-8601 UTC (`2026-11-04T12:56:31.000Z`, `+00:00`) → unix seconds.
fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time: String = time.chars().take(8).collect();
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn window_name(secs: i64) -> String {
    match secs {
        18_000 => "5-hour".into(),
        604_800 => "Weekly".into(),
        86_400 => "Daily".into(),
        s if s % 86_400 == 0 => format!("{}-day", s / 86_400),
        s if s % 3600 == 0 => format!("{}-hour", s / 3600),
        s => format!("{}m", s / 60),
    }
}

// ---- Claude ------------------------------------------------------------

const CLAUDE_TTL: i64 = 60;

/// Seconds since `path` was last modified, if it exists.
fn age_secs(path: &std::path::Path) -> Option<i64> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(modified.elapsed().map(|d| d.as_secs() as i64).unwrap_or(0))
}

/// `claude-usage.sh`'s shared backoff after a 429: `<until-epoch> <strikes>`.
fn read_backoff(path: &std::path::Path) -> (i64, u32) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut parts = text.split_whitespace();
    let until = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let strikes = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (until, strikes)
}

/// Back off 2m, 4m, 8m … up to 15m after the `strikes`-th 429 in a row.
fn backoff_secs(strikes: u32) -> i64 {
    (120_i64 << strikes.saturating_sub(1).min(4)).min(900)
}

fn claude() -> Option<Provider> {
    let creds = read_json(home().join(".claude/.credentials.json"))?;
    let oauth = &creds["claudeAiOauth"];
    let token = oauth["accessToken"].as_str()?.to_string();
    let plan = oauth["subscriptionType"].as_str().map(capitalize);
    // One API budget shared with claude-usage.sh (the prompt and the Claude
    // statusline): its cache, its last-attempt stamp and its 429 backoff, so
    // the two never both ask within a minute or keep a rate limit going.
    let dir = home().join(".cache/claude-usage");
    let cache = dir.join("usage.json");
    let stamp = dir.join("last-attempt");
    let backoff = dir.join("backoff");
    let (until, strikes) = read_backoff(&backoff);
    let cache_age = age_secs(&cache);
    let asked_recently = age_secs(&stamp).is_some_and(|age| age < CLAUDE_TTL);
    let fresh = cache_age.is_some_and(|age| age < CLAUDE_TTL);
    let mut note = None;
    let mut error = None;
    if !fresh && !asked_recently && now() >= until {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(&stamp, b"");
        match get(
            "https://api.anthropic.com/api/oauth/usage",
            &[
                format!("Authorization: Bearer {token}"),
                "anthropic-beta: oauth-2025-04-20".into(),
            ],
        ) {
            Ok(body) => {
                let tmp = dir.join("usage.json.sidebar.tmp");
                if std::fs::write(&tmp, body.to_string()).is_ok() {
                    let _ = std::fs::rename(&tmp, &cache);
                }
                let _ = std::fs::remove_file(&backoff);
            }
            Err(err) if err.starts_with("rate limited") => {
                let strikes = strikes + 1;
                let _ = std::fs::write(
                    &backoff,
                    format!("{} {strikes}\n", now() + backoff_secs(strikes)),
                );
            }
            Err(err) => error = Some(err),
        }
    }
    let body = read_json(cache.clone());
    // Say how old the numbers are once they are not from this minute.
    let (until, _) = read_backoff(&backoff);
    if let Some(age) = age_secs(&cache).filter(|age| *age >= CLAUDE_TTL * 2) {
        let mut text = format!("numbers from {} ago", short_age(age));
        if until > now() {
            text = format!("API busy · {text} · retry in {}", short_age(until - now()));
        }
        note = Some(text);
    }
    let windows = body.as_ref().map(claude_windows).unwrap_or_default();
    Some(Provider {
        name: "Claude",
        plan,
        error: error.or_else(|| body.is_none().then(|| "no usage data yet".into())),
        note,
        windows,
    })
}

/// `90` → `1m`, `7500` → `2h`, for "numbers from … ago".
fn short_age(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

fn claude_windows(body: &Value) -> Vec<Window> {
    let window = |v: &Value, label: &str, length: i64| -> Option<Window> {
        let used = v["utilization"].as_f64()?;
        let resets_at = v["resets_at"].as_str().and_then(parse_time);
        // A window whose reset has passed holds old numbers.
        if resets_at.is_some_and(|t| t <= now()) {
            return None;
        }
        Some(Window {
            label: label.into(),
            used,
            resets_at,
            length_secs: Some(length),
            limited: used >= 100.0,
        })
    };
    let mut out: Vec<Window> = [
        window(&body["five_hour"], "5-hour", 18_000),
        window(&body["seven_day"], "Weekly", 604_800),
    ]
    .into_iter()
    .flatten()
    .collect();
    for limit in body["limits"].as_array().into_iter().flatten() {
        if limit["kind"] != "weekly_scoped" {
            continue;
        }
        let Some(used) = limit["percent"].as_f64() else {
            continue;
        };
        let model = limit["scope"]["model"]["display_name"]
            .as_str()
            .unwrap_or("model");
        out.push(Window {
            label: format!("Weekly · {model}"),
            used,
            resets_at: limit["resets_at"].as_str().and_then(parse_time),
            length_secs: Some(604_800),
            limited: used >= 100.0,
        });
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

// ---- Codex -------------------------------------------------------------

fn codex() -> Option<Provider> {
    let (token, account) = codex_auth()?;
    let result = get(
        "https://chatgpt.com/backend-api/wham/usage",
        &[
            format!("Authorization: Bearer {token}"),
            format!("ChatGPT-Account-Id: {account}"),
            "User-Agent: herdr-sidebar".into(),
        ],
    );
    Some(match result {
        Ok(body) => Provider {
            name: "Codex",
            plan: body["plan_type"].as_str().map(capitalize),
            windows: codex_windows(&body),
            error: None,
            note: None,
        },
        Err(err) => Provider {
            name: "Codex",
            plan: None,
            windows: Vec::new(),
            error: Some(err),
            note: None,
        },
    })
}

/// The Codex CLI's own tokens (it refreshes them while in use), else pi's.
fn codex_auth() -> Option<(String, String)> {
    if let Some(auth) = read_json(home().join(".codex/auth.json")) {
        let tokens = &auth["tokens"];
        if let (Some(t), Some(a)) = (
            tokens["access_token"].as_str(),
            tokens["account_id"].as_str(),
        ) {
            return Some((t.into(), a.into()));
        }
    }
    let pi = read_json(home().join(".pi/agent/auth.json"))?;
    let entry = &pi["openai-codex"];
    Some((
        entry["access"].as_str()?.into(),
        entry["accountId"].as_str()?.into(),
    ))
}

fn codex_windows(body: &Value) -> Vec<Window> {
    let limit = &body["rate_limit"];
    let reached = limit["limit_reached"].as_bool().unwrap_or(false);
    ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| {
            let w = &limit[*key];
            let used = w["used_percent"].as_f64()?;
            let length = w["limit_window_seconds"].as_i64();
            Some(Window {
                label: length.map(window_name).unwrap_or_else(|| "Limit".into()),
                used,
                resets_at: w["reset_at"].as_i64(),
                length_secs: length,
                limited: used >= 100.0 || (reached && used >= 99.0),
            })
        })
        .collect()
}

// ---- OpenCode Go -------------------------------------------------------

fn opencode_go() -> Option<Provider> {
    let key = opencode_go_key()?;
    let result = get(
        "https://opencode.ai/zen/go/v1/usage",
        &[
            format!("Authorization: Bearer {key}"),
            "User-Agent: herdr-sidebar".into(),
        ],
    );
    Some(match result {
        Ok(body) => Provider {
            name: "OpenCode Go",
            plan: None,
            windows: opencode_windows(&body),
            error: None,
            note: None,
        },
        Err(err) => Provider {
            name: "OpenCode Go",
            plan: None,
            windows: Vec::new(),
            error: Some(err),
            note: None,
        },
    })
}

fn opencode_go_key() -> Option<String> {
    if let Ok(key) = std::env::var("OPENCODE_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key.trim().into());
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"));
    [
        data.join("opencode/auth.json"),
        home().join(".pi/agent/auth.json"),
    ]
    .into_iter()
    .filter_map(read_json)
    .find_map(|auth| {
        auth["opencode-go"]["key"]
            .as_str()
            .map(|k| k.trim().to_string())
    })
    .filter(|k| !k.is_empty())
}

fn opencode_windows(body: &Value) -> Vec<Window> {
    [
        ("rolling", "Rolling", Some(18_000)),
        ("weekly", "Weekly", Some(604_800)),
        ("monthly", "Monthly", None),
    ]
    .iter()
    .filter_map(|(key, label, length)| {
        let w = &body["usage"][*key];
        let used = w["percent"].as_f64()?;
        Some(Window {
            label: (*label).into(),
            used: used.clamp(0.0, 100.0),
            resets_at: w["resetsAt"].as_str().and_then(parse_time),
            length_secs: *length,
            limited: w["status"].as_str().is_some_and(|s| s != "ok") || used >= 100.0,
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_two_minutes_and_caps_at_fifteen() {
        assert_eq!(backoff_secs(1), 120);
        assert_eq!(backoff_secs(2), 240);
        assert_eq!(backoff_secs(3), 480);
        assert_eq!(backoff_secs(4), 900);
        assert_eq!(backoff_secs(40), 900);
    }

    #[test]
    fn iso_times_parse_to_unix_seconds() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-11-04T12:56:31.000Z"), Some(1_793_796_991));
        assert_eq!(
            parse_time("2026-10-21T16:59:59.914397+00:00"),
            Some(1_792_601_999)
        );
        assert_eq!(parse_time("garbage"), None);
    }

    #[test]
    fn codex_answers_become_windows() {
        let body = serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "limit_reached": false,
                "primary_window": {"used_percent": 37, "limit_window_seconds": 18000, "reset_at": 2_000_000_000},
                "secondary_window": {"used_percent": 12, "limit_window_seconds": 604800, "reset_at": 2_000_500_000},
            }
        });
        let w = codex_windows(&body);
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].label.as_str(), w[0].used), ("5-hour", 37.0));
        assert_eq!(
            (w[1].label.as_str(), w[1].length_secs),
            ("Weekly", Some(604_800))
        );
        assert!(
            codex_windows(&serde_json::json!({"rate_limit": {"secondary_window": null}}))
                .is_empty()
        );
    }

    #[test]
    fn opencode_rate_limited_windows_are_flagged() {
        let body = serde_json::json!({"usage": {
            "rolling": {"status": "ok", "percent": 0, "resetsAt": "2099-01-01T00:00:00Z"},
            "weekly": {"status": "rate-limited", "percent": 100, "resetsAt": "2099-01-02T00:00:00Z"},
            "monthly": {"status": "ok", "percent": 80, "resetsAt": "2099-01-30T00:00:00Z"},
        }});
        let w = opencode_windows(&body);
        assert_eq!(
            w.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
            ["Rolling", "Weekly", "Monthly"]
        );
        assert!(!w[0].limited && w[1].limited && !w[2].limited);
    }

    #[test]
    fn claude_answers_include_model_scoped_limits_and_drop_expired_windows() {
        let body = serde_json::json!({
            "five_hour": {"utilization": 27.0, "resets_at": "2099-01-01T05:00:00+00:00"},
            "seven_day": {"utilization": 54.0, "resets_at": "2000-01-01T00:00:00+00:00"},
            "limits": [
                {"kind": "session", "percent": 27},
                {"kind": "weekly_scoped", "percent": 64, "resets_at": "2099-01-07T00:00:00+00:00",
                 "scope": {"model": {"display_name": "Fable"}}}
            ]
        });
        let w = claude_windows(&body);
        assert_eq!(
            w.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
            ["5-hour", "Weekly · Fable"]
        );
    }
}
