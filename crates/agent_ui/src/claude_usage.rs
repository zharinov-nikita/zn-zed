//! Subscription usage limits for Claude Code threads.
//!
//! Claude Code exposes the same numbers its `/usage` command shows through
//! `GET https://api.anthropic.com/api/oauth/usage`, authenticated with the OAuth
//! access token the CLI stores locally. Nothing else (the CLI itself, the local
//! session transcripts) records those numbers, so we talk to the endpoint
//! directly and cache the result in this store.
//!
//! The endpoint rate limits aggressively, so we poll at most every few minutes,
//! back off on 429, and only keep polling while a thread is actually rendering
//! the indicator.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest, StatusCode};
use serde::Deserialize;
use ui::SharedString;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
/// The endpoint hands out instant, sticky 429s to unknown user agents.
const FALLBACK_CLI_VERSION: &str = "2.1.0";
const POLL_INTERVAL: Duration = Duration::from_secs(180);
const MIN_MANUAL_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// Stop polling once nothing has rendered the indicator for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(360);

/// Whether the agent behind a thread is Claude Code, and thus has subscription
/// limits we can show. Agent ids come from the ACP registry (`claude-acp`) or
/// from user settings, so match loosely.
pub fn is_claude_code_agent(agent_id: &project::AgentId) -> bool {
    agent_id.as_ref().to_lowercase().contains("claude")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSeverity {
    Normal,
    Warning,
    Critical,
}

impl UsageSeverity {
    fn from_percent(percent: f32) -> Self {
        if percent >= 95.0 {
            Self::Critical
        } else if percent >= 80.0 {
            Self::Warning
        } else {
            Self::Normal
        }
    }

    fn parse(raw: Option<&str>, percent: f32) -> Self {
        match raw {
            Some("critical") => Self::Critical,
            Some("warning") => Self::Warning,
            Some("normal") => Self::Normal,
            _ => Self::from_percent(percent),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    pub percent: f32,
    pub resets_at: Option<DateTime<Utc>>,
    pub severity: UsageSeverity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScopedUsageWindow {
    pub label: SharedString,
    pub window: UsageWindow,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtraUsage {
    pub used_credits: Option<f32>,
    pub monthly_limit: Option<f32>,
    pub percent: Option<f32>,
    pub currency: Option<SharedString>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeUsage {
    /// The rolling five hour window ("session" limit).
    pub session: Option<UsageWindow>,
    /// The weekly limit across all models.
    pub weekly: Option<UsageWindow>,
    /// Per-model weekly limits, e.g. Opus.
    pub scoped: Vec<ScopedUsageWindow>,
    /// Extra usage credits, only when the account has them enabled.
    pub extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FetchState {
    Idle,
    Fetching,
    /// The endpoint pushed back; don't try again before this instant.
    RateLimited {
        until: Instant,
    },
    /// The stored credentials were rejected; a new `claude` login is needed.
    Unauthorized,
    Error(SharedString),
}

pub struct ClaudeUsageStore {
    usage: Option<ClaudeUsage>,
    state: FetchState,
    fetched_at: Option<Instant>,
    last_rendered_at: Option<Instant>,
    /// No credentials file at all: the indicator stays hidden.
    credentials_missing: bool,
    backoff: Duration,
    _poll_task: Option<Task<()>>,
}

struct GlobalClaudeUsageStore(Entity<ClaudeUsageStore>);
impl Global for GlobalClaudeUsageStore {}

pub fn init(cx: &mut App) {
    ClaudeUsageStore::init_global(cx);
}

impl ClaudeUsageStore {
    fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalClaudeUsageStore>() {
            return;
        }
        let store = cx.new(|_| Self {
            usage: None,
            state: FetchState::Idle,
            fetched_at: None,
            last_rendered_at: None,
            credentials_missing: false,
            backoff: POLL_INTERVAL,
            _poll_task: None,
        });
        cx.set_global(GlobalClaudeUsageStore(store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalClaudeUsageStore>()
            .map(|global| global.0.clone())
    }

    pub fn usage(&self) -> Option<&ClaudeUsage> {
        self.usage.as_ref()
    }

    pub fn state(&self) -> &FetchState {
        &self.state
    }

    pub fn fetched_at(&self) -> Option<Instant> {
        self.fetched_at
    }

    /// True once we know there are no usable Claude Code credentials on this
    /// machine, so callers can hide the indicator entirely.
    pub fn credentials_missing(&self) -> bool {
        self.credentials_missing
    }

    /// Called from the indicator's render pass: keeps the polling task alive
    /// while the indicator is on screen and starts it on first paint.
    pub fn poll(&mut self, cx: &mut Context<Self>) {
        self.last_rendered_at = Some(Instant::now());
        if self._poll_task.is_none() {
            self.start_polling(cx);
        }
    }

    /// Manual refresh from a click. Respects an active rate limit backoff.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if let FetchState::RateLimited { until } = self.state
            && Instant::now() < until
        {
            return;
        }
        if self
            .fetched_at
            .is_some_and(|at| at.elapsed() < MIN_MANUAL_REFRESH_INTERVAL)
        {
            return;
        }
        self.last_rendered_at = Some(Instant::now());
        self._poll_task = None;
        self.start_polling(cx);
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self._poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let http_client = cx.update(|cx| cx.http_client());

                if this
                    .update(cx, |this, cx| {
                        this.state = FetchState::Fetching;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }

                let result = fetch_usage(http_client).await;

                let Ok(sleep_for) = this.update(cx, |this, cx| {
                    this.apply_result(result, cx);
                    this.backoff
                }) else {
                    break;
                };

                cx.background_executor().timer(sleep_for).await;

                let keep_going = this
                    .update(cx, |this, _| {
                        this.last_rendered_at
                            .is_some_and(|at| at.elapsed() < IDLE_TIMEOUT)
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }

            this.update(cx, |this, _| this._poll_task = None).ok();
        }));
    }

    fn apply_result(&mut self, result: Result<ClaudeUsage, FetchError>, cx: &mut Context<Self>) {
        match result {
            Ok(usage) => {
                self.usage = Some(usage);
                self.state = FetchState::Idle;
                self.fetched_at = Some(Instant::now());
                self.credentials_missing = false;
                self.backoff = POLL_INTERVAL;
            }
            Err(FetchError::NoCredentials) => {
                self.usage = None;
                self.credentials_missing = true;
                self.state = FetchState::Idle;
                self.backoff = MAX_BACKOFF;
            }
            Err(FetchError::Unauthorized) => {
                self.state = FetchState::Unauthorized;
                self.backoff = MAX_BACKOFF;
            }
            Err(FetchError::RateLimited { retry_after }) => {
                let backoff = retry_after.unwrap_or_else(|| next_backoff(self.backoff));
                self.backoff = backoff.min(MAX_BACKOFF);
                self.state = FetchState::RateLimited {
                    until: Instant::now() + self.backoff,
                };
            }
            Err(FetchError::Other(message)) => {
                log::warn!("failed to fetch Claude Code usage: {message}");
                self.state = FetchState::Error(message.into());
                self.backoff = next_backoff(self.backoff);
            }
        }
        cx.notify();
    }
}

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

#[derive(Debug)]
enum FetchError {
    NoCredentials,
    Unauthorized,
    RateLimited { retry_after: Option<Duration> },
    Other(String),
}

async fn fetch_usage(http_client: Arc<dyn HttpClient>) -> Result<ClaudeUsage, FetchError> {
    let credentials = read_credentials()
        .await
        .map_err(|err| FetchError::Other(err.to_string()))?
        .ok_or(FetchError::NoCredentials)?;

    if credentials.is_expired() {
        return Err(FetchError::Unauthorized);
    }

    let user_agent = format!(
        "claude-cli/{} (external, cli)",
        read_cli_version()
            .await
            .unwrap_or_else(|| { SharedString::new_static(FALLBACK_CLI_VERSION) })
    );

    let request = HttpRequest::builder()
        .method(Method::GET)
        .uri(USAGE_URL)
        .header("Authorization", format!("Bearer {}", credentials.token))
        .header("anthropic-beta", OAUTH_BETA_HEADER)
        .header("User-Agent", user_agent)
        .header("Content-Type", "application/json")
        .body(AsyncBody::default())
        .map_err(|err| FetchError::Other(err.to_string()))?;

    let mut response = http_client
        .send(request)
        .await
        .map_err(|err| FetchError::Other(err.to_string()))?;

    let status = response.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(FetchError::Unauthorized);
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(FetchError::RateLimited { retry_after });
    }

    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .map_err(|err| FetchError::Other(err.to_string()))?;

    if !status.is_success() {
        return Err(FetchError::Other(format!(
            "usage endpoint returned {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }

    parse_usage(&body).map_err(|err| FetchError::Other(err.to_string()))
}

// Wire types

#[derive(Debug, Deserialize)]
struct UsageResponse {
    #[serde(default)]
    five_hour: Option<LegacyWindow>,
    #[serde(default)]
    seven_day: Option<LegacyWindow>,
    #[serde(default)]
    seven_day_opus: Option<LegacyWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<LegacyWindow>,
    #[serde(default)]
    extra_usage: Option<ExtraUsageResponse>,
    #[serde(default)]
    limits: Vec<LimitEntry>,
}

#[derive(Debug, Deserialize)]
struct LegacyWindow {
    utilization: f32,
    #[serde(default)]
    resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct LimitEntry {
    kind: String,
    #[serde(default)]
    percent: f32,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    scope: Option<LimitScope>,
}

#[derive(Debug, Deserialize)]
struct LimitScope {
    #[serde(default)]
    model: Option<LimitScopeModel>,
    #[serde(default)]
    surface: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LimitScopeModel {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtraUsageResponse {
    #[serde(default)]
    is_enabled: bool,
    #[serde(default)]
    monthly_limit: Option<f32>,
    #[serde(default)]
    used_credits: Option<f32>,
    #[serde(default)]
    utilization: Option<f32>,
    #[serde(default)]
    currency: Option<String>,
}

fn parse_usage(body: &str) -> Result<ClaudeUsage> {
    let response: UsageResponse =
        serde_json::from_str(body).context("failed to parse usage response")?;

    let mut usage = ClaudeUsage::default();

    for limit in &response.limits {
        let window = UsageWindow {
            percent: limit.percent,
            resets_at: limit.resets_at,
            severity: UsageSeverity::parse(limit.severity.as_deref(), limit.percent),
        };

        match limit.kind.as_str() {
            "session" => usage.session = Some(window),
            "weekly_all" => usage.weekly = Some(window),
            _ => {
                let label = limit
                    .scope
                    .as_ref()
                    .and_then(|scope| {
                        scope
                            .model
                            .as_ref()
                            .and_then(|model| model.display_name.clone())
                            .or_else(|| scope.surface.clone())
                    })
                    .unwrap_or_else(|| humanize_limit_kind(&limit.kind));
                usage.scoped.push(ScopedUsageWindow {
                    label: label.into(),
                    window,
                });
            }
        }
    }

    // Older shapes of the response only carry the named windows.
    let legacy_window = |window: &LegacyWindow| UsageWindow {
        percent: window.utilization,
        resets_at: window.resets_at,
        severity: UsageSeverity::from_percent(window.utilization),
    };

    if usage.session.is_none() {
        usage.session = response.five_hour.as_ref().map(legacy_window);
    }
    if usage.weekly.is_none() {
        usage.weekly = response.seven_day.as_ref().map(legacy_window);
    }
    if usage.scoped.is_empty() {
        for (label, window) in [
            ("Opus", response.seven_day_opus.as_ref()),
            ("Sonnet", response.seven_day_sonnet.as_ref()),
        ] {
            if let Some(window) = window {
                usage.scoped.push(ScopedUsageWindow {
                    label: label.into(),
                    window: legacy_window(window),
                });
            }
        }
    }

    usage.extra_usage = response.extra_usage.and_then(|extra| {
        extra.is_enabled.then(|| ExtraUsage {
            used_credits: extra.used_credits,
            monthly_limit: extra.monthly_limit,
            percent: extra.utilization,
            currency: extra.currency.map(SharedString::from),
        })
    });

    if usage.session.is_none() && usage.weekly.is_none() && usage.scoped.is_empty() {
        anyhow::bail!("usage response contained no limits");
    }

    Ok(usage)
}

fn humanize_limit_kind(kind: &str) -> String {
    let mut label = String::new();
    for (index, word) in kind.split('_').enumerate() {
        if index > 0 {
            label.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            label.extend(first.to_uppercase());
            label.push_str(chars.as_str());
        }
    }
    label
}

// Credentials

struct Credentials {
    token: String,
    expires_at_ms: Option<i64>,
}

impl Credentials {
    fn is_expired(&self) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at| Utc::now().timestamp_millis() >= expires_at)
    }
}

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OAuthCredentials>,
}

#[derive(Debug, Deserialize)]
struct OAuthCredentials {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt", default)]
    expires_at: Option<i64>,
}

fn parse_credentials(contents: &str) -> Result<Credentials> {
    let file: CredentialsFile =
        serde_json::from_str(contents).context("failed to parse Claude Code credentials")?;
    let oauth = file
        .claude_ai_oauth
        .ok_or_else(|| anyhow!("credentials file has no claudeAiOauth entry"))?;
    Ok(Credentials {
        token: oauth.access_token,
        expires_at_ms: oauth.expires_at,
    })
}

async fn read_credentials() -> Result<Option<Credentials>> {
    let contents = smol::unblock(read_credentials_source).await?;
    let Some(contents) = contents else {
        return Ok(None);
    };
    parse_credentials(&contents).map(Some)
}

fn read_credentials_source() -> Result<Option<String>> {
    // Claude Code keeps the token in the login keychain on macOS, and falls
    // back to the same JSON file everywhere else.
    #[cfg(target_os = "macos")]
    if let Some(contents) = read_keychain_credentials() {
        return Ok(Some(contents));
    }

    let path = paths::home_dir().join(".claude").join(".credentials.json");
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

#[cfg(target_os = "macos")]
fn read_keychain_credentials() -> Option<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let contents = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!contents.is_empty()).then_some(contents)
}

/// Best effort read of the installed CLI version, used for the `User-Agent`
/// the usage endpoint expects.
///
/// `~/.claude.json` also holds the CLI's per-project history and routinely
/// grows to tens of megabytes, so the answer is read once per process instead
/// of on every poll.
async fn read_cli_version() -> Option<SharedString> {
    static CACHED: OnceLock<Option<SharedString>> = OnceLock::new();
    if let Some(cached) = CACHED.get() {
        return cached.clone();
    }

    let version = smol::unblock(|| {
        let path = paths::home_dir().join(".claude.json");
        let contents = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
        value
            .get("lastOnboardingVersion")?
            .as_str()
            .map(|version| SharedString::from(version.to_string()))
    })
    .await;

    CACHED.get_or_init(|| version).clone()
}

/// "2h 14m", "41m", "6d 3h" — compact enough for a tooltip line.
pub fn format_time_until(target: DateTime<Utc>) -> String {
    let seconds = (target - Utc::now()).num_seconds();
    if seconds <= 0 {
        return "now".to_string();
    }
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if days > 0 {
        format!("{}d {}h", days, hours % 24)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes % 60)
    } else if minutes > 0 {
        format!("{}m", minutes)
    } else {
        "<1m".to_string()
    }
}

/// "just now", "2m ago" — for the "last updated" tooltip footer.
pub fn format_time_since(instant: Instant) -> String {
    let seconds = instant.elapsed().as_secs();
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_RESPONSE: &str = r#"{
        "five_hour": {"utilization": 11.0, "resets_at": "2026-07-22T03:09:59.795349+00:00"},
        "seven_day": {"utilization": 61.0, "resets_at": "2026-07-26T06:59:59.795368+00:00"},
        "seven_day_opus": null,
        "seven_day_sonnet": null,
        "extra_usage": {"is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null},
        "limits": [
            {"kind": "session", "group": "session", "percent": 11, "severity": "normal", "resets_at": "2026-07-22T03:09:59.795349+00:00", "scope": null, "is_active": false},
            {"kind": "weekly_all", "group": "weekly", "percent": 61, "severity": "normal", "resets_at": "2026-07-26T06:59:59.795368+00:00", "scope": null, "is_active": false},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 100, "severity": "critical", "resets_at": "2026-07-26T06:59:59.881841+00:00", "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}, "is_active": true}
        ]
    }"#;

    #[test]
    fn parses_limits_array() {
        let usage = parse_usage(FULL_RESPONSE).unwrap();

        let session = usage.session.unwrap();
        assert_eq!(session.percent, 11.0);
        assert_eq!(session.severity, UsageSeverity::Normal);
        assert!(session.resets_at.is_some());

        assert_eq!(usage.weekly.unwrap().percent, 61.0);

        assert_eq!(usage.scoped.len(), 1);
        assert_eq!(usage.scoped[0].label, "Fable");
        assert_eq!(usage.scoped[0].window.severity, UsageSeverity::Critical);

        assert!(usage.extra_usage.is_none());
    }

    #[test]
    fn falls_back_to_named_windows() {
        let body = r#"{
            "five_hour": {"utilization": 33.0, "resets_at": "2026-04-11T07:00:00.528743+00:00"},
            "seven_day": {"utilization": 13.0, "resets_at": "2026-04-17T00:59:59.951713+00:00"},
            "seven_day_opus": null,
            "seven_day_sonnet": {"utilization": 1.0, "resets_at": "2026-04-16T03:00:00.951719+00:00"}
        }"#;

        let usage = parse_usage(body).unwrap();
        assert_eq!(usage.session.unwrap().percent, 33.0);
        assert_eq!(usage.weekly.unwrap().percent, 13.0);
        assert_eq!(usage.scoped.len(), 1);
        assert_eq!(usage.scoped[0].label, "Sonnet");
    }

    #[test]
    fn reports_extra_usage_only_when_enabled() {
        let body = r#"{
            "five_hour": {"utilization": 5.0},
            "extra_usage": {"is_enabled": true, "monthly_limit": 50.0, "used_credits": 12.5, "utilization": 25.0, "currency": "USD"}
        }"#;

        let extra = parse_usage(body).unwrap().extra_usage.unwrap();
        assert_eq!(extra.used_credits, Some(12.5));
        assert_eq!(extra.monthly_limit, Some(50.0));
        assert_eq!(extra.percent, Some(25.0));
        assert_eq!(extra.currency.unwrap(), "USD");
    }

    #[test]
    fn rejects_response_without_limits() {
        assert!(parse_usage(r#"{"five_hour": null, "seven_day": null}"#).is_err());
    }

    #[test]
    fn parses_credentials_and_detects_expiry() {
        let contents = r#"{"claudeAiOauth": {"accessToken": "token", "expiresAt": 1, "scopes": [], "subscriptionType": "max"}}"#;
        let credentials = parse_credentials(contents).unwrap();
        assert_eq!(credentials.token, "token");
        assert!(credentials.is_expired());

        let future = Utc::now().timestamp_millis() + 60_000;
        let contents =
            format!(r#"{{"claudeAiOauth": {{"accessToken": "token", "expiresAt": {future}}}}}"#);
        assert!(!parse_credentials(&contents).unwrap().is_expired());
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut backoff = POLL_INTERVAL;
        for _ in 0..10 {
            backoff = next_backoff(backoff);
        }
        assert_eq!(backoff, MAX_BACKOFF);
    }
}
