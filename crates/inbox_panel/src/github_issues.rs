//! Read-only GitHub issues mirror for the inbox panel.
//!
//! The open issues of the repository the inbox is bound to are fetched from
//! the GitHub REST API and shown in a separate panel section. Issues are
//! **not** [`InboxItem`](crate::inbox_model::InboxItem)s: they are never
//! persisted into the inbox document, carry no local read/cleared state, and
//! every mutation-looking control in the UI only re-reads GitHub. The last
//! successful response is cached in the key-value store (own key, see
//! [`issues_cache_key`]) so the section isn't empty on startup or offline.
//!
//! Auth is resolved once per binding: `gh auth token` → `GITHUB_TOKEN` env →
//! unauthenticated. The token lives in memory only and is dropped on a 401
//! and on rebind.

use std::sync::Arc;
use std::time::Duration;

use collections::HashSet;
use futures::AsyncReadExt as _;
use gpui::{AppContext as _, Context, Task};
use http_client::{HttpClient, HttpRequestExt as _, RedirectPolicy, Request};
use project::{ProjectPath, git_store::Repository};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;
use util::rel_path::RelPath;

use crate::inbox_model::{format_age, now_unix};
use crate::inbox_panel_settings::{InboxPanelSettings, Settings as _};
use crate::inbox_store::{INBOX_KV_NAMESPACE, InboxStore, InboxStoreEvent};

const GITHUB_API_URL: &str = "https://api.github.com";

/// Issues per REST page; pagination is driven by the `Link` header.
pub const GITHUB_PAGE_SIZE: usize = 100;

/// How many issues the KV cache keeps. Bounds the cache document size for
/// repositories with thousands of open issues.
const ISSUES_CACHE_CAP: usize = 200;

/// Version pinned into every cache write; newer-versioned cache documents are
/// skipped on load (mirroring the inbox document's policy).
const ISSUES_CACHE_VERSION: u32 = 1;

/// One open issue, as returned by `GET /repos/{owner}/{repo}/issues`.
/// `Serialize` is deliberate: the same struct is the KV cache document entry
/// and the MCP tool's structured output.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GithubIssue {
    pub number: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<GithubLabel>,
    /// `None` for deleted ("ghost") users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<GithubUser>,
    /// `"open"` — the fetch filters on it, but keep it for the MCP output.
    pub state: String,
    /// ISO-8601 (RFC 3339), parsed lazily via [`Self::created_unix`].
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
    /// Comment count (the comments themselves are fetched lazily by the
    /// detail view).
    #[serde(default)]
    pub comments: u64,
    /// Present only on pull requests — the issues endpoint returns PRs too.
    /// Used to filter them out and never persisted.
    #[serde(default, skip_serializing)]
    pub pull_request: Option<serde_json::Value>,
}

impl GithubIssue {
    pub fn created_unix(&self) -> Option<i64> {
        parse_rfc3339_unix(&self.created_at)
    }

    pub fn updated_unix(&self) -> Option<i64> {
        parse_rfc3339_unix(&self.updated_at)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GithubLabel {
    pub name: String,
    /// Hex RGB without the leading `#`, e.g. `"d73a4a"`.
    #[serde(default)]
    pub color: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GithubUser {
    pub login: String,
}

/// One issue comment, fetched lazily when the detail view opens. Never
/// persisted anywhere.
#[derive(Deserialize, Clone, Debug)]
pub struct GithubComment {
    #[serde(default)]
    pub user: Option<GithubUser>,
    pub created_at: String,
    #[serde(default)]
    pub body: Option<String>,
}

impl GithubComment {
    pub fn created_unix(&self) -> Option<i64> {
        parse_rfc3339_unix(&self.created_at)
    }
}

fn parse_rfc3339_unix(text: &str) -> Option<i64> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|datetime| datetime.unix_timestamp())
}

/// Why a fetch failed, mapped to a short user-facing message. Any error keeps
/// the last successful list on screen.
#[derive(Debug)]
pub enum GithubFetchError {
    /// 403/429 with the rate-limit budget exhausted. Unauthenticated requests
    /// share 60/hour per IP, so the message points at authenticating.
    RateLimited,
    /// 404 — misspelled repo, or a private repo without (sufficient) auth.
    NotFound,
    /// 401 — the token is bad; the caller drops the cached token so the next
    /// refresh re-resolves it.
    Unauthorized,
    /// Transport-level failure (offline, DNS, TLS).
    Network(anyhow::Error),
    /// Any other non-success status.
    Api { status: u16 },
}

impl std::fmt::Display for GithubFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GithubFetchError::RateLimited => write!(
                f,
                "GitHub rate limit exceeded — run `gh auth login` or set GITHUB_TOKEN"
            ),
            GithubFetchError::NotFound => {
                write!(f, "repository not found on GitHub (private repo without auth?)")
            }
            GithubFetchError::Unauthorized => write!(f, "GitHub rejected the token (401)"),
            GithubFetchError::Network(error) => write!(f, "network error: {error:#}"),
            GithubFetchError::Api { status } => write!(f, "GitHub API error (status {status})"),
        }
    }
}

/// Fetches one page of open issues, newest-updated first. Returns the issues
/// with pull requests filtered out, plus whether another page exists (from
/// the `Link` header, so an exact-multiple issue count can't fake it).
pub async fn fetch_issues_page(
    http: Arc<dyn HttpClient>,
    token: Option<&str>,
    owner: &str,
    repo: &str,
    page: u32,
) -> Result<(Vec<GithubIssue>, bool), GithubFetchError> {
    let url = format!(
        "{GITHUB_API_URL}/repos/{owner}/{repo}/issues\
         ?state=open&sort=updated&direction=desc&per_page={GITHUB_PAGE_SIZE}&page={page}"
    );
    let response = github_get(http, token, &url).await?;
    let issues = issues_page_from_json(&response.body).map_err(|error| {
        GithubFetchError::Network(anyhow::anyhow!("error deserializing issues: {error}"))
    })?;
    Ok((issues, response.has_next_page))
}

/// Fetches the comments of one issue (first 100 — enough for a panel-sized
/// read-only view).
pub async fn fetch_issue_comments(
    http: Arc<dyn HttpClient>,
    token: Option<&str>,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<Vec<GithubComment>, GithubFetchError> {
    let url =
        format!("{GITHUB_API_URL}/repos/{owner}/{repo}/issues/{number}/comments?per_page=100");
    let response = github_get(http, token, &url).await?;
    serde_json::from_slice(&response.body).map_err(|error| {
        GithubFetchError::Network(anyhow::anyhow!("error deserializing comments: {error}"))
    })
}

struct GithubResponse {
    body: Vec<u8>,
    /// Whether the `Link` header advertises a `rel="next"` page.
    has_next_page: bool,
}

/// One authenticated-when-possible GET against the GitHub API, returning the
/// raw body on success and a classified [`GithubFetchError`] otherwise.
/// Modeled on [`http_client::github::latest_github_release`].
async fn github_get(
    http: Arc<dyn HttpClient>,
    token: Option<&str>,
    url: &str,
) -> Result<GithubResponse, GithubFetchError> {
    let request = Request::get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .follow_redirects(RedirectPolicy::FollowAll)
        .when_some(token, |builder, token| {
            builder.header("Authorization", format!("Bearer {token}"))
        })
        .body(Default::default())
        .map_err(|error| GithubFetchError::Network(error.into()))?;

    let mut response = http
        .send(request)
        .await
        .map_err(GithubFetchError::Network)?;

    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .map_err(|error| GithubFetchError::Network(error.into()))?;

    let status = response.status();
    if status.is_success() {
        let has_next_page = response
            .headers()
            .get("link")
            .and_then(|value| value.to_str().ok())
            .is_some_and(link_header_has_next);
        return Ok(GithubResponse {
            body,
            has_next_page,
        });
    }
    let rate_limit_remaining = response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok());
    Err(classify_error_status(status.as_u16(), rate_limit_remaining))
}

/// Whether a `Link` header value contains a `rel="next"` entry, e.g.
/// `<https://api.github.com/...&page=2>; rel="next", <...>; rel="last"`.
fn link_header_has_next(link: &str) -> bool {
    link.split(',')
        .any(|entry| entry.split(';').skip(1).any(|param| {
            param.trim().eq_ignore_ascii_case("rel=\"next\"")
        }))
}

/// Maps a non-success HTTP status to a [`GithubFetchError`]. A 403 is only a
/// rate limit when the budget header says so — GitHub also uses it for
/// forbidden resources.
fn classify_error_status(status: u16, rate_limit_remaining: Option<&str>) -> GithubFetchError {
    match status {
        401 => GithubFetchError::Unauthorized,
        403 if rate_limit_remaining == Some("0") => GithubFetchError::RateLimited,
        429 => GithubFetchError::RateLimited,
        404 => GithubFetchError::NotFound,
        status => GithubFetchError::Api { status },
    }
}

/// Parses one raw issues page and filters out pull requests. Pagination is
/// judged by the caller from the `Link` header, not from the page length.
fn issues_page_from_json(body: &[u8]) -> Result<Vec<GithubIssue>, serde_json::Error> {
    let mut issues: Vec<GithubIssue> = serde_json::from_slice(body)?;
    issues.retain(|issue| issue.pull_request.is_none());
    Ok(issues)
}

/// Resolves a GitHub token: `gh auth token` (GitHub CLI) → `GITHUB_TOKEN`
/// env → `None` (unauthenticated; fine for public repos, 60 req/h per IP).
/// `pub(crate)` for the detail view's comments fetch.
pub(crate) async fn resolve_github_token() -> Option<String> {
    if let Some(token) = gh_cli_token().await {
        return Some(token);
    }
    std::env::var("GITHUB_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

async fn gh_cli_token() -> Option<String> {
    // `new_command` sets CREATE_NO_WINDOW on Windows, so no console flashes.
    // A `gh` installed as a non-exe shim won't resolve; the env fallback
    // covers that.
    let output = util::command::new_command("gh")
        .args(["auth", "token"])
        .stdin(util::command::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}

/// The in-memory auth cache: resolved once per binding, dropped on 401 and
/// on rebind. Never persisted.
#[derive(Clone, Debug, Default, PartialEq)]
enum TokenState {
    #[default]
    Unresolved,
    /// `None` means "resolved to unauthenticated" — don't re-run `gh` on
    /// every refresh.
    Resolved(Option<String>),
}

/// What the bound worktree's remote turned out to be.
enum RemoteProbe {
    /// No worktree/repository/remote URL yet — retry on git-store events.
    Unknown,
    /// A remote exists but doesn't point at public github.com.
    NotGithub,
    Github { owner: String, repo: String },
}

/// Which GitHub repository the issues section mirrors, if any.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum GithubBinding {
    /// No remote URL seen yet — remotes arrive asynchronously after the repo
    /// scan, so binding is retried on git-store events.
    #[default]
    Unresolved,
    /// The project has no github.com remote (including self-hosted GitHub,
    /// which the REST paths here don't cover). The section is hidden.
    NoGithubRemote,
    Bound {
        owner: String,
        repo: String,
    },
}

/// The issues mirror of one bound project. Owned by [`InboxStore`] (so the
/// panel and the MCP server read the same state) and reset wholesale on
/// rebind — dropping `_poll` kills the timer, and the bumped generation in
/// the fresh default invalidates in-flight fetches.
#[derive(Default)]
pub struct GithubIssuesState {
    binding: GithubBinding,
    /// `Arc` so the render path can hold the list across a frame without
    /// deep-cloning issue bodies; mutations go through [`Arc::make_mut`]
    /// (no other holder survives between frames, so no hidden copies).
    issues: Arc<Vec<GithubIssue>>,
    /// Unix seconds of the last successful *live* fetch, or the cached
    /// response's own fetch time while `from_cache`.
    fetched_at: Option<i64>,
    /// Whether `issues` still comes from the KV cache (no live fetch has
    /// succeeded for this binding yet).
    from_cache: bool,
    /// A page-1 fetch is in flight.
    loading: bool,
    /// A "show more" page fetch is in flight.
    loading_more: bool,
    error: Option<String>,
    has_more: bool,
    /// Next page for "show more"; page 1 is the refresh.
    next_page: u32,
    token: TokenState,
    /// Invalidates in-flight fetches across rebinds, mirroring the store's
    /// `load_generation`.
    fetch_generation: u64,
    _poll: Option<Task<()>>,
}

impl GithubIssuesState {
    pub fn binding(&self) -> &GithubBinding {
        &self.binding
    }

    /// The bound `owner/repo`, if the project has a github.com remote.
    pub fn owner_repo(&self) -> Option<(&str, &str)> {
        match &self.binding {
            GithubBinding::Bound { owner, repo } => Some((owner, repo)),
            _ => None,
        }
    }

    /// Open issues, newest-updated first, PRs excluded.
    pub fn issues(&self) -> &[GithubIssue] {
        &self.issues
    }

    /// A cheap handle on the list for the render path.
    pub(crate) fn issues_arc(&self) -> Arc<Vec<GithubIssue>> {
        self.issues.clone()
    }

    pub fn fetched_at(&self) -> Option<i64> {
        self.fetched_at
    }

    pub fn from_cache(&self) -> bool {
        self.from_cache
    }

    pub fn loading(&self) -> bool {
        self.loading
    }

    pub fn loading_more(&self) -> bool {
        self.loading_more
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn has_more(&self) -> bool {
        self.has_more
    }

    /// Whether the section should be shown at all. Cached issues are only
    /// ever adopted after a successful bind, so "bound" covers them too.
    pub fn is_visible(&self) -> bool {
        matches!(self.binding, GithubBinding::Bound { .. })
    }

    /// One shared human-readable freshness line for the panel tooltip and
    /// the MCP tool — honest about whether a refresh is actually running
    /// (`from_cache` alone can't tell: it stays set when the kicked refresh
    /// failed or was skipped by the staleness gate).
    pub fn freshness_line(&self, now: i64) -> String {
        match (self.fetched_at, self.from_cache, self.loading) {
            (None, _, true) => "fetching".to_string(),
            (None, _, false) => "not fetched yet".to_string(),
            (Some(at), true, true) => format!("cached {} ago, refreshing", format_age(at, now)),
            (Some(at), true, false) => format!("cached {} ago", format_age(at, now)),
            (Some(at), false, true) => format!("updated {} ago, refreshing", format_age(at, now)),
            (Some(at), false, false) => format!("updated {} ago", format_age(at, now)),
        }
    }

    /// The already-resolved token (`Some(None)` = resolved to
    /// unauthenticated), or `None` while resolution hasn't happened yet.
    /// Lets the detail view's comments fetch reuse the list fetch's auth.
    pub(crate) fn cached_token(&self) -> Option<Option<String>> {
        match &self.token {
            TokenState::Resolved(token) => Some(token.clone()),
            TokenState::Unresolved => None,
        }
    }
}

/// Key of a project's issues cache in the [`INBOX_KV_NAMESPACE`] scope.
/// Distinct from the 16-hex inbox document keys, so the backup ring and the
/// migration probe never see it.
fn issues_cache_key(project_key: &str) -> String {
    format!("github_issues:{project_key}")
}

/// The persisted cache document: the last successful response (capped),
/// stamped with the binding it belongs to so a repo switch under the same
/// project key can't surface foreign issues.
#[derive(Serialize, Deserialize)]
struct IssuesCacheDoc {
    version: u32,
    owner: String,
    repo: String,
    fetched_at: i64,
    issues: Vec<GithubIssue>,
}

/// Parses a cache document, refusing one written by a newer Zed (mirroring
/// the inbox document's load policy).
fn parse_cache_doc(text: &str) -> Option<IssuesCacheDoc> {
    let doc: IssuesCacheDoc = serde_json::from_str(text).ok()?;
    (doc.version <= ISSUES_CACHE_VERSION).then_some(doc)
}

impl InboxStore {
    pub fn github_issues(&self) -> &GithubIssuesState {
        &self.github_issues
    }

    /// Memoizes a token resolved outside the list fetch (the detail view's
    /// comments fetch), so the store's own next fetch doesn't spawn `gh`
    /// again.
    pub(crate) fn memoize_github_token(&mut self, token: Option<String>) {
        if self.github_issues.token == TokenState::Unresolved {
            self.github_issues.token = TokenState::Resolved(token);
        }
    }

    /// Replaces the issues state for a rebind: drops the poll timer, the
    /// token and the list wholesale, but keeps the fetch generation monotonic
    /// (a plain `Default` would reset it to zero, and a stale in-flight fetch
    /// from the old binding could then match a freshly bumped generation and
    /// land its foreign issues into the new binding).
    pub(crate) fn reset_github_issues(&mut self) {
        let generation = self.github_issues.fetch_generation;
        self.github_issues = GithubIssuesState {
            fetch_generation: generation + 1,
            ..Default::default()
        };
    }

    /// Derives the GitHub binding from the bound worktree's repository and,
    /// on success, loads the KV cache, starts the poll timer and triggers the
    /// initial fetch. Safe to call repeatedly: re-runs are cheap while the
    /// binding is unresolved and no-ops once bound.
    pub(crate) fn bind_github_issues(&mut self, cx: &mut Context<Self>) {
        if !InboxPanelSettings::get_global(cx).github_issues_enabled {
            return;
        }
        if self.rebinding {
            // A worktree rebind's flush is still pending: `bound_project_key`
            // names the *outgoing* project, so binding now would read (and
            // write!) the issues cache under a foreign key. The deferred
            // rebind path re-runs this after the reload.
            return;
        }
        if matches!(self.github_issues.binding, GithubBinding::Bound { .. }) {
            return;
        }
        match self.github_owner_repo(cx) {
            RemoteProbe::Github { owner, repo } => {
                self.github_issues.binding = GithubBinding::Bound { owner, repo };
                self.load_issues_cache(cx);
                self.start_issues_poll(cx);
                self.refresh_github_issues(true, cx);
                cx.emit(InboxStoreEvent::GithubIssuesUpdated);
            }
            RemoteProbe::NotGithub => {
                // Terminal for now, but a later git event re-probes (cheap),
                // so switching `origin` to github.com mid-session recovers.
                self.github_issues.binding = GithubBinding::NoGithubRemote;
            }
            RemoteProbe::Unknown => {}
        }
    }

    /// `owner/repo` of the bound worktree's `origin` (preferred — for a fork
    /// you want *your* issues, not upstream's) or `upstream` remote, when it
    /// points at github.com.
    fn github_owner_repo(&self, cx: &Context<Self>) -> RemoteProbe {
        let Some(worktree_id) = self.worktree_id else {
            return RemoteProbe::Unknown;
        };
        let project_path = ProjectPath {
            worktree_id,
            path: RelPath::empty().into(),
        };
        let Some((repository, _)) = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .repository_and_path_for_project_path(&project_path, cx)
        else {
            return RemoteProbe::Unknown;
        };
        let repository: &Repository = repository.read(cx);
        let Some(url) = repository
            .remote_origin_url
            .clone()
            .or_else(|| repository.remote_upstream_url.clone())
        else {
            // Remote URLs arrive asynchronously after the repo scan.
            return RemoteProbe::Unknown;
        };
        // `try_global`: the registry is set at app startup
        // (`git_hosting_providers::init`); absent only in tests, where the
        // binding then simply stays unresolved.
        let Some(registry) = git::GitHostingProviderRegistry::try_global(cx) else {
            return RemoteProbe::Unknown;
        };
        let Some((provider, remote)) = git::parse_git_remote_url(registry, &url) else {
            return RemoteProbe::NotGithub;
        };
        // Public github.com only: GHES serves the REST API under `/api/v3`,
        // which the URLs in this module don't cover.
        if provider.base_url().host_str() != Some("github.com") {
            return RemoteProbe::NotGithub;
        }
        RemoteProbe::Github {
            owner: remote.owner.to_string(),
            repo: remote.repo.to_string(),
        }
    }

    /// Re-fetches page 1. With `force` false (the poll timer and the MCP
    /// tool), skips while the last fetch is younger than the poll interval;
    /// the manual refresh button passes true. Also retries the binding, so a
    /// remote added after open is picked up by the next poll tick.
    pub fn refresh_github_issues(&mut self, force: bool, cx: &mut Context<Self>) {
        if !InboxPanelSettings::get_global(cx).github_issues_enabled {
            return;
        }
        if !matches!(self.github_issues.binding, GithubBinding::Bound { .. }) {
            self.bind_github_issues(cx);
            return;
        }
        if self.github_issues.loading {
            return;
        }
        if !force && self.github_issues.loading_more {
            // Let the in-flight "show more" land instead of invalidating it
            // via the generation bump; the next poll tick refreshes page 1.
            return;
        }
        if !force
            && self.github_issues.fetched_at.is_some_and(|fetched_at| {
                let age = now_unix().saturating_sub(fetched_at);
                // A negative age (fetched_at in the future after a backwards
                // clock step) must count as stale, not forever-fresh.
                (0..issues_poll_interval(cx).as_secs() as i64).contains(&age)
            })
        {
            return;
        }
        self.github_issues.loading = true;
        self.spawn_issues_fetch(1, cx);
    }

    /// Fetches the next page and appends it ("Show more").
    pub fn load_more_github_issues(&mut self, cx: &mut Context<Self>) {
        if self.github_issues.loading
            || self.github_issues.loading_more
            || !self.github_issues.has_more
        {
            return;
        }
        self.github_issues.loading_more = true;
        let page = self.github_issues.next_page;
        self.spawn_issues_fetch(page, cx);
    }

    /// The shared fetch task: resolves the token when needed, fetches one
    /// page and lands the result through [`Self::finish_issues_fetch`].
    /// The state's flags (`loading`/`loading_more`) are set by the caller.
    fn spawn_issues_fetch(&mut self, page: u32, cx: &mut Context<Self>) {
        let GithubBinding::Bound { owner, repo } = self.github_issues.binding.clone() else {
            return;
        };
        self.github_issues.fetch_generation += 1;
        let generation = self.github_issues.fetch_generation;
        let token_state = self.github_issues.token.clone();
        let http = cx.http_client();
        cx.emit(InboxStoreEvent::GithubIssuesUpdated);
        cx.spawn(async move |this, cx| {
            let token = match token_state {
                TokenState::Resolved(token) => token,
                TokenState::Unresolved => resolve_github_token().await,
            };
            let result = fetch_issues_page(http, token.as_deref(), &owner, &repo, page).await;
            this.update(cx, |this, cx| {
                this.finish_issues_fetch(generation, page, token, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_issues_fetch(
        &mut self,
        generation: u64,
        page: u32,
        token: Option<String>,
        result: Result<(Vec<GithubIssue>, bool), GithubFetchError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.github_issues.fetch_generation {
            // A rebind (or a newer fetch) happened while this one was in
            // flight; its result belongs to a binding that no longer exists.
            return;
        }
        self.github_issues.loading = false;
        self.github_issues.loading_more = false;
        self.github_issues.token = TokenState::Resolved(token);
        match result {
            Ok((issues, has_more)) => {
                if page == 1 {
                    if self.github_issues.next_page > 2 {
                        // The user paged deeper: keep the tail instead of
                        // collapsing their expansion — fresh page first,
                        // then the previously loaded remainder (dedup by
                        // number). A closed issue can linger in the tail
                        // until the next Show-more cycle; the tail is
                        // best-effort by design. `next_page`/`has_more`
                        // still describe the deepest loaded page.
                        let fresh: HashSet<u64> =
                            issues.iter().map(|issue| issue.number).collect();
                        let previous = std::mem::take(&mut self.github_issues.issues);
                        let mut merged = issues;
                        merged.extend(
                            previous
                                .iter()
                                .filter(|issue| !fresh.contains(&issue.number))
                                .cloned(),
                        );
                        self.github_issues.issues = Arc::new(merged);
                    } else {
                        self.github_issues.issues = Arc::new(issues);
                        self.github_issues.has_more = has_more;
                        self.github_issues.next_page = 2;
                    }
                    // Only a page-1 fetch refreshes the *list*; a deeper
                    // page must not reset the staleness clock or the
                    // "updated X ago" freshness claim.
                    self.github_issues.fetched_at = Some(now_unix());
                } else {
                    // The list may have shifted between pages; dedup by
                    // number, first (newer) occurrence wins.
                    let seen: HashSet<u64> = self
                        .github_issues
                        .issues
                        .iter()
                        .map(|issue| issue.number)
                        .collect();
                    Arc::make_mut(&mut self.github_issues.issues).extend(
                        issues
                            .into_iter()
                            .filter(|issue| !seen.contains(&issue.number)),
                    );
                    self.github_issues.has_more = has_more;
                    self.github_issues.next_page = page + 1;
                }
                self.github_issues.from_cache = false;
                self.github_issues.error = None;
                self.persist_issues_cache(cx);
            }
            Err(error) => {
                if matches!(error, GithubFetchError::Unauthorized) {
                    // Drop the bad token so the next refresh re-resolves it
                    // (the user may have re-run `gh auth login` meanwhile).
                    self.github_issues.token = TokenState::Unresolved;
                }
                // Keep the last successful list — an error only annotates it.
                self.github_issues.error = Some(error.to_string());
            }
        }
        cx.emit(InboxStoreEvent::GithubIssuesUpdated);
    }

    /// Adopts the cached response of a previous run, so the section isn't
    /// empty while the first live fetch runs (or fails offline). Adopted only
    /// while no live data has landed and only when the cached binding matches
    /// the current one.
    fn load_issues_cache(&self, cx: &mut Context<Self>) {
        let Some(project_key) = self.bound_project_key.clone() else {
            return;
        };
        let key_value_store = self.key_value_store.clone();
        cx.spawn(async move |this, cx| {
            // The sync KV read hits SQLite; keep it off the UI thread.
            let text = cx
                .background_spawn(async move {
                    key_value_store
                        .scoped(INBOX_KV_NAMESPACE)
                        .read(&issues_cache_key(&project_key))
                })
                .await
                .log_err()
                .flatten()?;
            let doc = parse_cache_doc(&text)?;
            this.update(cx, |this, cx| {
                if this.github_issues.fetched_at.is_some() {
                    // A live fetch (or another cache load) already landed.
                    return;
                }
                if this.github_issues.owner_repo() != Some((doc.owner.as_str(), doc.repo.as_str()))
                {
                    return;
                }
                this.github_issues.issues = Arc::new(doc.issues);
                this.github_issues.fetched_at = Some(doc.fetched_at);
                this.github_issues.from_cache = true;
                cx.emit(InboxStoreEvent::GithubIssuesUpdated);
            })
            .ok()
        })
        .detach();
    }

    /// Writes the current list (capped) to the cache key. Independent of the
    /// inbox document's save path: no debounce, no `dirty`/`pending_writes` —
    /// those guard the document key only.
    fn persist_issues_cache(&self, cx: &mut Context<Self>) {
        let Some(project_key) = self.bound_project_key.clone() else {
            return;
        };
        let Some((owner, repo)) = self.github_issues.owner_repo() else {
            return;
        };
        let Some(fetched_at) = self.github_issues.fetched_at else {
            return;
        };
        let doc = IssuesCacheDoc {
            version: ISSUES_CACHE_VERSION,
            owner: owner.to_string(),
            repo: repo.to_string(),
            fetched_at,
            issues: self
                .github_issues
                .issues
                .iter()
                .take(ISSUES_CACHE_CAP)
                .cloned()
                .collect(),
        };
        let key_value_store = self.key_value_store.clone();
        cx.background_spawn(async move {
            let write = async {
                let text = serde_json::to_string(&doc)?;
                key_value_store
                    .scoped(INBOX_KV_NAMESPACE)
                    .write(issues_cache_key(&project_key), text)
                    .await
            };
            if let Err(error) = write.await {
                log::warn!("inbox: failed to write the github issues cache: {error:#}");
            }
        })
        .detach();
    }

    /// The background poll loop, owned by the store so it keeps the mirror
    /// fresh (for the MCP tool) while the panel is hidden. The interval is
    /// re-read every tick, so a settings change applies without a restart.
    fn start_issues_poll(&mut self, cx: &mut Context<Self>) {
        self.github_issues._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(interval) = this.update(cx, |_, cx| issues_poll_interval(cx)) else {
                    break;
                };
                cx.background_executor().timer(interval).await;
                if this
                    .update(cx, |this, cx| this.refresh_github_issues(false, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
}

fn issues_poll_interval(cx: &gpui::App) -> Duration {
    let minutes = InboxPanelSettings::get_global(cx)
        .github_issues_poll_minutes
        .max(1);
    // `saturating_mul` backs up the settings-side clamp: an absurd
    // `poll_minutes` must not overflow into a panic or a near-zero interval.
    Duration::from_secs(minutes.saturating_mul(60))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    use fs::FakeFs;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use http_client::FakeHttpClient;
    use pretty_assertions::assert_eq;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    use super::*;

    fn issue_json(number: u64, pull_request: bool) -> serde_json::Value {
        let mut issue = json!({
            "number": number,
            "title": format!("Issue {number}"),
            "state": "open",
            "created_at": "2026-07-01T10:00:00Z",
            "updated_at": "2026-07-19T08:30:00Z",
            "html_url": format!("https://github.com/octo/repo/issues/{number}"),
            "comments": 4,
            "labels": [{ "name": "bug", "color": "d73a4a" }],
            "user": { "login": "octocat" },
            "body": "Something is broken",
        });
        if pull_request {
            issue["pull_request"] = json!({ "url": "https://api.github.com/..." });
        }
        issue
    }

    #[test]
    fn test_issues_page_filters_pull_requests() {
        let body =
            serde_json::to_vec(&json!([issue_json(1, false), issue_json(2, true)])).unwrap();
        let issues = issues_page_from_json(&body).unwrap();
        assert_eq!(issues.len(), 1);
        let issue = &issues[0];
        assert_eq!(issue.number, 1);
        assert_eq!(issue.title, "Issue 1");
        assert_eq!(issue.body.as_deref(), Some("Something is broken"));
        assert_eq!(issue.labels.len(), 1);
        assert_eq!(issue.labels[0].name, "bug");
        assert_eq!(issue.labels[0].color, "d73a4a");
        assert_eq!(issue.user.as_ref().unwrap().login, "octocat");
        assert_eq!(issue.comments, 4);
        // 2026-07-01T10:00:00Z, cross-checked against an independent
        // RFC 3339 implementation.
        assert_eq!(issue.created_unix(), Some(1_782_900_000));
        assert!(issue.updated_unix().is_some());
    }

    #[test]
    fn test_issues_page_tolerates_ghost_user_and_missing_fields() {
        let body = serde_json::to_vec(&json!([{
            "number": 7,
            "title": "Ghost",
            "state": "open",
            "created_at": "2026-07-01T10:00:00Z",
            "updated_at": "2026-07-01T10:00:00Z",
            "html_url": "https://github.com/octo/repo/issues/7",
            "user": null,
            "body": null,
        }]))
        .unwrap();
        let issues = issues_page_from_json(&body).unwrap();
        assert_eq!(issues[0].user, None);
        assert_eq!(issues[0].body, None);
        assert_eq!(issues[0].labels, Vec::new());
        assert_eq!(issues[0].comments, 0);
    }

    #[test]
    fn test_link_header_drives_pagination() {
        assert!(link_header_has_next(
            "<https://api.github.com/repos/o/r/issues?page=2>; rel=\"next\", \
             <https://api.github.com/repos/o/r/issues?page=9>; rel=\"last\""
        ));
        // Last page: only prev/first remain.
        assert!(!link_header_has_next(
            "<https://api.github.com/repos/o/r/issues?page=8>; rel=\"prev\", \
             <https://api.github.com/repos/o/r/issues?page=1>; rel=\"first\""
        ));
        assert!(!link_header_has_next(""));
        // A PR-heavy page still parses; pagination no longer keys off its
        // length, so an exact-multiple issue count can't fake a next page.
        let entries: Vec<_> = (0..GITHUB_PAGE_SIZE as u64)
            .map(|number| issue_json(number, number % 5 == 0))
            .collect();
        let body = serde_json::to_vec(&entries).unwrap();
        let issues = issues_page_from_json(&body).unwrap();
        assert_eq!(issues.len(), 80);
    }

    #[test]
    fn test_classify_error_status() {
        assert!(matches!(
            classify_error_status(401, None),
            GithubFetchError::Unauthorized
        ));
        assert!(matches!(
            classify_error_status(403, Some("0")),
            GithubFetchError::RateLimited
        ));
        // A 403 with budget left is not a rate limit.
        assert!(matches!(
            classify_error_status(403, Some("54")),
            GithubFetchError::Api { status: 403 }
        ));
        assert!(matches!(
            classify_error_status(429, None),
            GithubFetchError::RateLimited
        ));
        assert!(matches!(
            classify_error_status(404, None),
            GithubFetchError::NotFound
        ));
        assert!(matches!(
            classify_error_status(500, None),
            GithubFetchError::Api { status: 500 }
        ));
    }

    #[test]
    fn test_cache_doc_roundtrip_and_version_gate() {
        let body = serde_json::to_vec(&json!([issue_json(1, false)])).unwrap();
        let issues = issues_page_from_json(&body).unwrap();
        let doc = IssuesCacheDoc {
            version: ISSUES_CACHE_VERSION,
            owner: "octo".into(),
            repo: "repo".into(),
            fetched_at: 123,
            issues,
        };
        let text = serde_json::to_string(&doc).unwrap();
        // The transient `pull_request` marker must never be persisted.
        assert!(!text.contains("pull_request"));
        let parsed = parse_cache_doc(&text).unwrap();
        assert_eq!(parsed.owner, "octo");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.fetched_at, 123);
        assert_eq!(parsed.issues, doc.issues);

        // A newer-versioned cache is skipped, not loaded.
        let newer = text.replace(
            &format!("\"version\":{ISSUES_CACHE_VERSION}"),
            &format!("\"version\":{}", ISSUES_CACHE_VERSION + 1),
        );
        assert!(parse_cache_doc(&newer).is_none());
        // Garbage is skipped too.
        assert!(parse_cache_doc("not json").is_none());
    }

    // Store-level tests. They live here (not in `inbox_store.rs`) because a
    // `FakeFs` project has no GitHub remote: the tests bind the mirror by
    // writing the private state directly, which only this module can do.
    // Pre-resolving the token also keeps `gh` from being spawned in tests.

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // A fresh in-memory database per test, matching `inbox_store.rs`.
            cx.set_global(db::AppDatabase::test_new());
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn build_bound_store(
        issues_response: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<InboxStore>) {
        // Serialize up front: FakeHttpClient handlers must be Send.
        let body = serde_json::to_string(&issues_response).unwrap();
        cx.update(|cx| {
            cx.set_http_client(FakeHttpClient::create(move |_request| {
                let body = body.clone();
                async move {
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(body.into())
                        .unwrap())
                }
            }));
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref() as &Path], cx).await;
        let store = cx.new(|cx| InboxStore::new(project.clone(), fs, cx));
        cx.run_until_parked();
        store.update(cx, |store, _| {
            store.github_issues.binding = GithubBinding::Bound {
                owner: "octo".to_string(),
                repo: "repo".to_string(),
            };
            store.github_issues.token = TokenState::Resolved(None);
        });
        (project, store)
    }

    fn issues_events(store: &Entity<InboxStore>, cx: &mut TestAppContext) -> Rc<RefCell<usize>> {
        let count = Rc::new(RefCell::new(0));
        let captured = count.clone();
        cx.update(|cx| {
            cx.subscribe(store, move |_, event: &InboxStoreEvent, _| {
                if matches!(event, InboxStoreEvent::GithubIssuesUpdated) {
                    *captured.borrow_mut() += 1;
                }
            })
            .detach();
        });
        count
    }

    #[gpui::test]
    async fn test_refresh_populates_state_and_cache(cx: &mut TestAppContext) {
        init_test(cx);
        let (_project, store) =
            build_bound_store(json!([issue_json(1, false), issue_json(2, true)]), cx).await;
        let events = issues_events(&store, cx);

        store.update(cx, |store, cx| store.refresh_github_issues(true, cx));
        assert!(store.read_with(cx, |store, _| store.github_issues().loading()));
        cx.run_until_parked();

        let key = store.read_with(cx, |store, _| {
            let state = store.github_issues();
            assert!(!state.loading());
            assert!(!state.from_cache());
            assert_eq!(state.error(), None);
            assert!(state.fetched_at().is_some());
            // The PR was filtered out.
            assert_eq!(
                state
                    .issues()
                    .iter()
                    .map(|issue| issue.number)
                    .collect::<Vec<_>>(),
                [1]
            );
            store.bound_project_key().unwrap().to_string()
        });
        // Started + finished.
        assert!(*events.borrow() >= 2);

        // The cache landed under the issues key, disjoint from the document.
        let key_value_store = cx.update(|cx| db::kvp::KeyValueStore::global(cx));
        let cached = key_value_store
            .scoped(INBOX_KV_NAMESPACE)
            .read(&issues_cache_key(&key))
            .unwrap()
            .expect("issues cache should be written");
        let doc = parse_cache_doc(&cached).unwrap();
        assert_eq!(doc.owner, "octo");
        assert_eq!(doc.issues.len(), 1);
        // The inbox document key itself is untouched by the issues fetch.
        assert_eq!(key_value_store.scoped(INBOX_KV_NAMESPACE).read(&key).unwrap(), None);
    }

    #[gpui::test]
    async fn test_fetch_error_keeps_previous_list(cx: &mut TestAppContext) {
        init_test(cx);
        let (_project, store) = build_bound_store(json!([issue_json(1, false)]), cx).await;

        store.update(cx, |store, cx| store.refresh_github_issues(true, cx));
        cx.run_until_parked();
        assert_eq!(
            store.read_with(cx, |store, _| store.github_issues().issues().len()),
            1
        );

        // Swap the transport for a failing one and force a refresh: the list
        // must survive, annotated with the error.
        cx.update(|cx| {
            cx.set_http_client(FakeHttpClient::create(|_request| async move {
                Ok(http_client::Response::builder()
                    .status(500)
                    .body(String::new().into())
                    .unwrap())
            }));
        });
        store.update(cx, |store, cx| store.refresh_github_issues(true, cx));
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let state = store.github_issues();
            assert_eq!(state.issues().len(), 1);
            assert!(state.error().unwrap().contains("500"));
        });
    }

    #[gpui::test]
    async fn test_rebind_drops_stale_in_flight_fetch(cx: &mut TestAppContext) {
        init_test(cx);
        let (_project, store) = build_bound_store(json!([issue_json(1, false)]), cx).await;

        // Start a fetch but reset the binding before the executor runs it:
        // the landed result must be dropped, not adopted by the new binding.
        store.update(cx, |store, cx| store.refresh_github_issues(true, cx));
        store.update(cx, |store, _| {
            store.reset_github_issues();
            store.github_issues.binding = GithubBinding::Bound {
                owner: "other".to_string(),
                repo: "repo".to_string(),
            };
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let state = store.github_issues();
            assert_eq!(state.issues().len(), 0);
            assert_eq!(state.fetched_at(), None);
        });
    }

    #[gpui::test]
    async fn test_page_one_refresh_keeps_paged_tail(cx: &mut TestAppContext) {
        init_test(cx);
        let (_project, store) = build_bound_store(json!([]), cx).await;

        store.update(cx, |store, cx| {
            // Simulate a list expanded to page 2: issues 1-3 loaded, page 3
            // would be next.
            let parse = |number| {
                serde_json::from_value::<GithubIssue>(issue_json(number, false)).unwrap()
            };
            store.github_issues.issues = Arc::new(vec![parse(1), parse(2), parse(3)]);
            store.github_issues.next_page = 3;
            store.github_issues.has_more = true;
            let generation = store.github_issues.fetch_generation;
            // A page-1 refresh lands: fresh copy of #2 plus a new #4.
            store.finish_issues_fetch(
                generation,
                1,
                None,
                Ok((vec![parse(4), parse(2)], false)),
                cx,
            );
            let state = store.github_issues();
            // Fresh page first, then the paged tail minus duplicates; the
            // expansion (and its pagination cursor) survives the refresh.
            assert_eq!(
                state
                    .issues()
                    .iter()
                    .map(|issue| issue.number)
                    .collect::<Vec<_>>(),
                [4, 2, 1, 3]
            );
            assert_eq!(store.github_issues.next_page, 3);
            assert!(store.github_issues.has_more);
        });
    }

    #[test]
    fn test_freshness_line_reports_actual_activity() {
        let mut state = GithubIssuesState::default();
        assert_eq!(state.freshness_line(1000), "not fetched yet");
        state.loading = true;
        assert_eq!(state.freshness_line(1000), "fetching");
        // A cache-adopted list whose kicked refresh failed or was skipped
        // must NOT claim a refresh is in flight.
        state.loading = false;
        state.fetched_at = Some(400);
        state.from_cache = true;
        assert_eq!(state.freshness_line(1000), "cached 10m ago");
        state.loading = true;
        assert_eq!(state.freshness_line(1000), "cached 10m ago, refreshing");
        state.from_cache = false;
        state.loading = false;
        assert_eq!(state.freshness_line(1000), "updated 10m ago");
    }
}
