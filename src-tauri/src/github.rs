// GitHub pulse and contribution calendar — the pure half of GithubPoller.swift,
// GitHubPulse.swift and GitHubActivity.swift. Parsing, alert detection and the
// staleness rule live here, away from the network, so they can be tested.
//
// The pollers that use them are in integrations.rs. The one request made from
// here is the card's pull requests through the local `gh` login (at the end).

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::chat::program::{self, Handle, Spec};
use crate::i18n::{t, tf};
use crate::integrations::alerts::{parse_watch, repo_ok, watched, Alert};
use crate::net;

// ── CI and review states ──────────────────────────────────────────────────────

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CiState {
    Pending,
    Success,
    Failure,
    Unknown,
}

impl CiState {
    /// GitHub's `StatusState` (statusCheckRollup.state) → our four states.
    pub fn from_github(raw: Option<&str>) -> Self {
        match raw.map(str::to_ascii_uppercase).as_deref() {
            Some("PENDING") | Some("EXPECTED") => CiState::Pending,
            Some("SUCCESS") => CiState::Success,
            Some("ERROR") | Some("FAILURE") => CiState::Failure,
            _ => CiState::Unknown,
        }
    }
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Pending,
    Unknown,
}

impl ReviewState {
    pub fn from_github(raw: Option<&str>) -> Self {
        match raw.map(str::to_ascii_uppercase).as_deref() {
            Some("APPROVED") => ReviewState::Approved,
            Some("CHANGES_REQUESTED") => ReviewState::ChangesRequested,
            Some("REVIEW_REQUIRED") => ReviewState::Pending,
            _ => ReviewState::Unknown,
        }
    }
}

// ── Pulse ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPr {
    /// "owner/repo#number"
    pub id: String,
    pub title: String,
    pub url: String,
    pub repo: String,
    pub number: i64,
    pub is_draft: bool,
    pub ci: CiState,
    pub review: ReviewState,
    /// Oid of the PR's last commit, when GitHub returned it.
    pub head_sha: Option<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GitHubRepoCi {
    pub repo: String,
    pub url: String,
    pub branch: String,
    pub ci: CiState,
    /// Oid of the default branch's head commit, when GitHub returned it.
    pub head_sha: Option<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct GitHubPulse {
    pub login: String,
    #[serde(rename = "myPRs")]
    pub my_prs: Vec<GitHubPr>,
    #[serde(rename = "toReview")]
    pub to_review: Vec<GitHubPr>,
    #[serde(rename = "mainCI")]
    pub main_ci: Vec<GitHubRepoCi>,
    /// CI of the branch a session is on, when it is not the default one.
    #[serde(rename = "branchCI")]
    pub branch_ci: Vec<GitHubRepoCi>,
    /// The GitLab or Gitea host merged into this pulse.
    #[serde(rename = "forgeHost", skip_serializing_if = "Option::is_none")]
    pub forge_host: Option<String>,
    /// Unix time in milliseconds.
    #[serde(rename = "fetchedAt")]
    pub fetched_at: u64,
}

/// What a pulse poll can raise: the island turns these into a badge and a sound.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum GitHubEvent {
    #[serde(rename = "ciFailed")]
    CiFailed {
        #[serde(rename = "prId")]
        pr_id: String,
    },
    #[serde(rename = "ciPassed")]
    CiPassed {
        #[serde(rename = "prId")]
        pr_id: String,
    },
    #[serde(rename = "mainFailed")]
    MainFailed { repo: String },
    #[serde(rename = "reviewRequested")]
    ReviewRequested {
        #[serde(rename = "prId")]
        pr_id: String,
    },
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Pull request fields shared by "my PRs" and "to review"; None when a required
/// field is missing (that node is skipped, as on macOS).
fn pr_basics(node: &Value) -> Option<(String, String, String, String, i64, bool)> {
    let number = node.get("number")?.as_i64()?;
    let title = str_at(node, "title")?.to_string();
    let url = str_at(node, "url")?.to_string();
    let repo = node.get("repository").and_then(|r| str_at(r, "nameWithOwner"))?.to_string();
    let is_draft = node.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    Some((format!("{repo}#{number}"), title, url, repo, number, is_draft))
}

fn nodes<'a>(parent: Option<&'a Value>) -> &'a [Value] {
    parent
        .and_then(|p| p.get("nodes"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

impl GitHubPulse {
    /// Parses the pulse GraphQL response. None when there is no `data.viewer`.
    pub fn parse(root: &Value, now_ms: u64) -> Option<Self> {
        let data = root.get("data")?.as_object()?;
        let viewer = data.get("viewer")?.as_object()?;
        let login = viewer.get("login").and_then(Value::as_str).unwrap_or("").to_string();

        // My pull requests, first occurrence of an id wins.
        let mut my_prs: Vec<GitHubPr> = Vec::new();
        for node in nodes(viewer.get("pullRequests")) {
            let Some((id, title, url, repo, number, is_draft)) = pr_basics(node) else { continue };
            if my_prs.iter().any(|p| p.id == id) {
                continue;
            }
            let review = ReviewState::from_github(str_at(node, "reviewDecision"));
            let commit = node
                .get("commits")
                .and_then(|c| c.get("nodes"))
                .and_then(Value::as_array)
                .and_then(|a| a.last())
                .and_then(|n| n.get("commit"));
            let ci_raw = commit
                .and_then(|c| c.get("statusCheckRollup"))
                .and_then(|r| str_at(r, "state"));
            let head_sha = commit.and_then(|c| str_at(c, "oid")).map(str::to_string);
            my_prs.push(GitHubPr {
                id,
                title,
                url,
                repo,
                number,
                is_draft,
                ci: CiState::from_github(ci_raw),
                review,
                head_sha,
            });
        }

        // Default-branch CI of the recently pushed repos, archived ones left out.
        let mut main_ci: Vec<GitHubRepoCi> = Vec::new();
        // The recent ones, then the watch list's, which the query asks by name (r0, r1…).
        let aliased = data
            .iter()
            .filter(|(k, _)| k.len() > 1 && k.starts_with('r') && k[1..].chars().all(|c| c.is_ascii_digit()))
            .map(|(_, v)| v);
        for node in nodes(viewer.get("repositories")).iter().chain(aliased) {
            if node.get("isArchived").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let (Some(repo), Some(url)) = (str_at(node, "nameWithOwner"), str_at(node, "url")) else {
                continue;
            };
            let Some(branch_ref) = node.get("defaultBranchRef").filter(|b| b.is_object()) else {
                continue;
            };
            let Some(branch) = str_at(branch_ref, "name") else { continue };
            if main_ci.iter().any(|r| r.repo == repo) {
                continue;
            }
            let target = branch_ref.get("target");
            let ci_raw = target
                .and_then(|t| t.get("statusCheckRollup"))
                .and_then(|r| str_at(r, "state"));
            let head_sha = target.and_then(|t| str_at(t, "oid")).map(str::to_string);
            main_ci.push(GitHubRepoCi {
                repo: repo.to_string(),
                url: url.to_string(),
                branch: branch.to_string(),
                ci: CiState::from_github(ci_raw),
                head_sha,
            });
        }

        // Pull requests waiting for my review.
        let mut to_review: Vec<GitHubPr> = Vec::new();
        for node in nodes(data.get("reviewRequested")) {
            let Some((id, title, url, repo, number, is_draft)) = pr_basics(node) else { continue };
            if to_review.iter().any(|p| p.id == id) {
                continue;
            }
            to_review.push(GitHubPr {
                id,
                title,
                url,
                repo,
                number,
                is_draft,
                ci: CiState::Unknown,
                review: ReviewState::Pending,
                head_sha: None,
            });
        }

        // The branches the sessions are on (b0, b1…), when not the default one.
        let mut branch_ci: Vec<GitHubRepoCi> = Vec::new();
        for (_, node) in data.iter().filter(|(k, _)| k.len() > 1 && k.starts_with('b') && k[1..].chars().all(|c| c.is_ascii_digit())) {
            let (Some(repo), Some(url)) = (str_at(node, "nameWithOwner"), str_at(node, "url")) else { continue };
            let Some(branch_ref) = node.get("ref").filter(|b| b.is_object()) else { continue };
            let Some(branch) = str_at(branch_ref, "name") else { continue };
            let default = node.get("defaultBranchRef").and_then(|b| str_at(b, "name"));
            if default == Some(branch) || branch_ci.iter().any(|r| r.repo == repo && r.branch == branch) {
                continue;
            }
            let target = branch_ref.get("target");
            branch_ci.push(GitHubRepoCi {
                repo: repo.to_string(),
                url: url.to_string(),
                branch: branch.to_string(),
                ci: CiState::from_github(target.and_then(|t| t.get("statusCheckRollup")).and_then(|r| str_at(r, "state"))),
                head_sha: target.and_then(|t| str_at(t, "oid")).map(str::to_string),
            });
        }

        Some(GitHubPulse { login, my_prs, to_review, main_ci, branch_ci, forge_host: None, fetched_at: now_ms })
    }

    /// True when a PR's CI or a default branch is still running — the next poll
    /// then comes after 60 s instead of 5 min.
    pub fn has_pending(&self) -> bool {
        self.my_prs.iter().any(|p| p.ci == CiState::Pending)
            || self.main_ci.iter().any(|r| r.ci == CiState::Pending)
            || self.branch_ci.iter().any(|r| r.ci == CiState::Pending)
    }

    /// With a watch list, only what belongs to it stays (cards and alerts alike).
    pub fn retain_watched(&mut self, watch: &[String]) {
        self.my_prs.retain(|p| watched(watch, &p.repo));
        self.to_review.retain(|p| watched(watch, &p.repo));
        self.main_ci.retain(|r| watched(watch, &r.repo));
        self.branch_ci.retain(|r| watched(watch, &r.repo));
    }

    /// Alerts between two polls. The first poll after launch (or after the token
    /// changed) has no `old` and stays silent.
    ///
    /// headSha rule, which catches CI runs faster than the poll interval:
    /// - same commit (or both unknown): plain state transitions;
    /// - new commit or new PR: alert at once if its CI is already done; if it is
    ///   still running, the next poll on the same commit will catch the result;
    /// - default branches only ever raise "failed", never "passed".
    pub fn events(old: Option<&GitHubPulse>, new: &GitHubPulse) -> Vec<GitHubEvent> {
        let Some(old) = old else { return Vec::new() };
        let mut out = Vec::new();

        for pr in &new.my_prs {
            // First occurrence wins, should the old list hold a duplicate.
            let prev = old.my_prs.iter().find(|p| p.id == pr.id);
            match prev {
                Some(prev) if prev.head_sha == pr.head_sha => {
                    if pr.ci == CiState::Failure && prev.ci != CiState::Failure {
                        out.push(GitHubEvent::CiFailed { pr_id: pr.id.clone() });
                    } else if pr.ci == CiState::Success && prev.ci == CiState::Pending {
                        out.push(GitHubEvent::CiPassed { pr_id: pr.id.clone() });
                    }
                }
                _ => match pr.ci {
                    CiState::Success => out.push(GitHubEvent::CiPassed { pr_id: pr.id.clone() }),
                    CiState::Failure => out.push(GitHubEvent::CiFailed { pr_id: pr.id.clone() }),
                    _ => {}
                },
            }
        }

        for repo in &new.main_ci {
            let prev = old.main_ci.iter().find(|r| r.repo == repo.repo);
            let failed = match prev {
                Some(prev) if prev.head_sha == repo.head_sha => {
                    repo.ci == CiState::Failure && prev.ci != CiState::Failure
                }
                _ => repo.ci == CiState::Failure,
            };
            if failed {
                out.push(GitHubEvent::MainFailed { repo: repo.repo.clone() });
            }
        }

        for pr in &new.to_review {
            if !old.to_review.iter().any(|p| p.id == pr.id) {
                out.push(GitHubEvent::ReviewRequested { pr_id: pr.id.clone() });
            }
        }

        out
    }
}

/// True when nothing was fetched yet or the last fetch is older than `max_age_secs`.
pub fn is_stale(fetched_at_ms: Option<u64>, now_ms: u64, max_age_secs: u64) -> bool {
    match fetched_at_ms {
        None => true,
        Some(t) => now_ms.saturating_sub(t) > max_age_secs * 1000,
    }
}

/// The notices a poll raises: CI failing on a pull request of mine or on a
/// default branch, a review asked of me, a pull request of mine that now waits
/// on me (changes requested, or approved). `events` is `events(old, new)`.
/// One key per run (the commit) or request, for the once-only rule.
pub fn alerts(old: Option<&GitHubPulse>, new: &GitHubPulse, events: &[GitHubEvent]) -> Vec<Alert> {
    let mut out = Vec::new();
    for event in events {
        match event {
            GitHubEvent::CiFailed { pr_id } => {
                if let Some(pr) = new.my_prs.iter().find(|p| &p.id == pr_id) {
                    let sha = pr.head_sha.clone().unwrap_or_default();
                    out.push(Alert { key: format!("ci:{pr_id}:{sha}"), kind: "ciFailed", bad: true, subject: pr.title.clone(), detail: pr.repo.clone(), url: pr.url.clone() });
                }
            }
            GitHubEvent::MainFailed { repo } => {
                if let Some(r) = new.main_ci.iter().find(|r| &r.repo == repo) {
                    let sha = r.head_sha.clone().unwrap_or_default();
                    out.push(Alert { key: format!("main:{repo}:{sha}"), kind: "ciFailed", bad: true, subject: r.repo.clone(), detail: r.branch.clone(), url: format!("{}/actions", r.url) });
                }
            }
            GitHubEvent::ReviewRequested { pr_id } => {
                if let Some(pr) = new.to_review.iter().find(|p| &p.id == pr_id) {
                    out.push(Alert { key: format!("review:{pr_id}"), kind: "review", bad: false, subject: pr.title.clone(), detail: pr.repo.clone(), url: pr.url.clone() });
                }
            }
            GitHubEvent::CiPassed { .. } => {}
        }
    }
    for pr in &new.my_prs {
        let Some(prev) = old.and_then(|o| o.my_prs.iter().find(|p| p.id == pr.id)) else { continue };
        let kind = match pr.review {
            ReviewState::ChangesRequested if prev.review != pr.review => "changes",
            ReviewState::Approved if prev.review != pr.review => "approved",
            _ => continue,
        };
        out.push(Alert { key: format!("{kind}:{}", pr.id), kind, bad: kind == "changes", subject: pr.title.clone(), detail: pr.repo.clone(), url: pr.url.clone() });
    }
    out
}

// ── Contribution calendar ─────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ContributionDay {
    /// YYYY-MM-DD
    pub date: String,
    pub count: i64,
    /// 0…4, from NONE … FOURTH_QUARTILE.
    pub level: u8,
    /// 0 = Sunday … 6 = Saturday.
    pub weekday: u8,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct GitHubActivity {
    pub total: i64,
    /// Oldest week first; the last one is usually the current, incomplete week.
    pub weeks: Vec<Vec<ContributionDay>>,
    #[serde(rename = "fetchedAt")]
    pub fetched_at: u64,
}

fn contribution_level(raw: &str) -> u8 {
    match raw {
        "FIRST_QUARTILE" => 1,
        "SECOND_QUARTILE" => 2,
        "THIRD_QUARTILE" => 3,
        "FOURTH_QUARTILE" => 4,
        _ => 0,
    }
}

impl GitHubActivity {
    pub fn parse(root: &Value, now_ms: u64) -> Option<Self> {
        let calendar = root
            .get("data")?
            .get("viewer")?
            .get("contributionsCollection")?
            .get("contributionCalendar")?;
        let total = calendar.get("totalContributions")?.as_i64()?;
        let weeks_raw = calendar.get("weeks")?.as_array()?;

        let mut weeks = Vec::new();
        for week in weeks_raw {
            let Some(days_raw) = week.get("contributionDays").and_then(Value::as_array) else { continue };
            let days: Vec<ContributionDay> = days_raw
                .iter()
                .filter_map(|d| {
                    Some(ContributionDay {
                        date: str_at(d, "date")?.to_string(),
                        count: d.get("contributionCount")?.as_i64()?,
                        level: contribution_level(str_at(d, "contributionLevel")?),
                        weekday: u8::try_from(d.get("weekday")?.as_i64()?).ok()?,
                    })
                })
                .collect();
            if !days.is_empty() {
                weeks.push(days);
            }
        }
        Some(GitHubActivity { total, weeks, fetched_at: now_ms })
    }
}

// ── GraphQL ───────────────────────────────────────────────────────────────────

/// Same query as GithubPoller.swift.
pub const PULSE_QUERY: &str = r#"
query {
  viewer {
    login
    pullRequests(states: OPEN, first: 20, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        number title url isDraft reviewDecision
        repository { nameWithOwner url }
        commits(last: 1) {
          nodes { commit { oid statusCheckRollup { state } } }
        }
      }
    }
    repositories(first: 10, ownerAffiliations: [OWNER], orderBy: {field: PUSHED_AT, direction: DESC}) {
      nodes {
        nameWithOwner url isArchived
        defaultBranchRef {
          name
          target { ... on Commit { oid statusCheckRollup { state } } }
        }
      }
    }
  }
  reviewRequested: search(query: "is:pr is:open review-requested:@me archived:false", type: ISSUE, first: 20) {
    issueCount
    nodes {
      ... on PullRequest {
        number title url isDraft
        author { login }
        repository { nameWithOwner url }
      }
    }
  }
}
"#;

/// The pulse query, with the watched repositories asked for by name.
pub fn pulse_query(watch: &[String]) -> String {
    let mut query = PULSE_QUERY.trim_end().to_string();
    query.pop(); // the closing brace
    for (i, (repo, branch)) in session_branches().iter().enumerate() {
        let (owner, name) = repo.split_once('/').unwrap_or_default();
        query.push_str(&format!(
            "b{i}: repository(owner: \"{owner}\", name: \"{name}\") {{ nameWithOwner url defaultBranchRef {{ name }} ref(qualifiedName: \"refs/heads/{branch}\") {{ name target {{ ... on Commit {{ oid statusCheckRollup {{ state }} }} }} }} }}
"
        ));
    }
    for (i, repo) in watch.iter().enumerate().filter(|(_, r)| repo_ok(r)) {
        let (owner, name) = repo.split_once('/').unwrap_or_default();
        query.push_str(&format!(
            "r{i}: repository(owner: \"{owner}\", name: \"{name}\") {{ nameWithOwner url isArchived defaultBranchRef {{ name target {{ ... on Commit {{ oid statusCheckRollup {{ state }} }} }} }} }}\n"
        ));
    }
    query.push('}');
    query
}

pub const ACTIVITY_QUERY: &str = r#"
query {
  viewer {
    login
    contributionsCollection {
      contributionCalendar {
        totalContributions
        weeks {
          contributionDays {
            date contributionCount contributionLevel weekday
          }
        }
      }
    }
  }
}
"#;

/// Number of GraphQL errors in a response, for the log (never their text: it
/// can quote the query, not secrets, but there is no need for it).
pub fn graphql_error_count(root: &Value) -> usize {
    root.get("errors").and_then(Value::as_array).map(Vec::len).unwrap_or(0)
}

// ── Branches of the sessions ──────────────────────────────────────────────────
//
// The island tells which folders its sessions work in (`github_refresh`); the
// branch and the origin are read from the folder's `.git` files, no git process.

static SESSION_DIRS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub fn set_session_dirs(dirs: Vec<String>) {
    *SESSION_DIRS.lock().unwrap() = dirs.into_iter().take(8).collect();
}

/// "owner/name" of a github.com remote address.
fn github_repo_of(url: &str) -> Option<&str> {
    let rest = ["https://github.com/", "git@github.com:", "ssh://git@github.com/"]
        .iter()
        .find_map(|p| url.strip_prefix(p))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    repo_ok(rest).then_some(rest)
}

/// (repo, branch) of the checkout at or above `dir`; None for a detached head,
/// another host or a worktree (debt: `.git` as a file is not followed).
fn git_branch_of(dir: &str) -> Option<(String, String)> {
    let mut at = std::path::Path::new(dir);
    while !at.join(".git").is_dir() {
        at = at.parent()?;
    }
    let git = at.join(".git");
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    // The branch goes into a query: plain name characters only.
    if branch.is_empty() || !branch.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c)) {
        return None;
    }
    let config = std::fs::read_to_string(git.join("config")).ok()?;
    let mut in_origin = false;
    let mut url = None;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line.starts_with("[remote \"origin\"]");
        } else if in_origin {
            if let Some(v) = line.strip_prefix("url").and_then(|v| v.trim_start().strip_prefix('=')) {
                url = Some(v.trim().to_string());
            }
        }
    }
    Some((github_repo_of(&url?)?.to_string(), branch.to_string()))
}

fn session_branches() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for found in SESSION_DIRS.lock().unwrap().iter().filter_map(|d| git_branch_of(d)) {
        if out.len() < 5 && !out.contains(&found) {
            out.push(found);
        }
    }
    out
}

// ── The local `gh` login ──────────────────────────────────────────────────────
//
// With "Use the gh login" on in Settings → Integrations → GitHub, and no token
// saved there, the card's pull requests are asked for with the login of the
// GitHub CLI (the idea is upstream pull request 15's). `gh auth token` is run
// when the card comes on screen; what it prints is used for that one request
// and dropped. It is never put in the credential store, in a file, in a static
// or in the log, so `gh auth logout` disconnects Lintel too.
//
// The background pollers of integrations.rs ask the same way when no token is
// saved (`github_token` there), once per request.

/// What Settings stored with the integrations feature.
/// debt: read from the file, because the command this question rides
/// (`chat::ask`) and the pollers have no way to the settings in memory.
fn integrations_setting(key: &str) -> Option<Value> {
    std::fs::read(crate::platform::config_dir().join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|settings| settings.pointer(&format!("/features/integrations/{key}")).cloned())
}

/// "Use the gh login", as Settings stored it.
pub fn gh_login_on() -> bool {
    integrations_setting("ghLogin").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// The watch list of repositories ("owner/name"), at most 20; empty = the user's own activity.
pub fn watch_repos() -> Vec<String> {
    parse_watch(integrations_setting("watchRepos").as_ref(), repo_ok)
}

/// The Vercel projects to watch, at most 20; empty = all of them.
pub fn watch_projects() -> Vec<String> {
    parse_watch(integrations_setting("watchProjects").as_ref(), |s| {
        !s.is_empty() && s.len() <= 100 && !s.contains(['"', '\\', '\n'])
    })
}

/// "Alert when a Vercel deploy is Ready or Failed" (opt-in).
pub fn vercel_alerts_on() -> bool {
    integrations_setting("vercelAlerts").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// The GitHub CLI's login for github.com, asked for now. Blocking.
pub fn gh_token() -> Result<String, String> {
    let gh = program::resolve(&["gh"]).ok_or_else(|| t("The GitHub CLI (gh) is not installed."))?;
    let spec = Spec {
        args: ["auth", "token", "--hostname", "github.com"].map(String::from).to_vec(),
        stdin: None,
        cwd: std::env::temp_dir(),
        timeout: Duration::from_secs(10),
        max_output: 4096,
    };
    let mut token = String::new();
    let run = program::run(&gh, &spec, &Handle::default(), |line| {
        if token.is_empty() {
            token = line.trim().to_string();
        }
    });
    match run {
        Ok(output) if output.success && !token.is_empty() => Ok(token),
        _ => Err(t("gh is not signed in. Run gh auth login in a terminal.")),
    }
}

/// The GitHub card came on screen: `{ "pulse": … }` fetched with the `gh`
/// login, or null when that is not what the user set up (the switch is off, a
/// token is saved and its poller answers, or Lintel is paused).
pub async fn pulse_through_gh() -> Result<Value, String> {
    if notch_core::paused() || !gh_login_on() || crate::secrets::present("github-token") {
        return Ok(Value::Null);
    }
    let watch = watch_repos();
    let token = tauri::async_runtime::spawn_blocking(gh_token).await.map_err(|err| err.to_string())??;
    let endpoint = reqwest::Url::parse("https://api.github.com/graphql").map_err(|err| err.to_string())?;
    let response = net::keyed_client(&endpoint, Duration::from_secs(15))?
        .post(endpoint)
        .bearer_auth(&token)
        .header("User-Agent", notch_brand::SLUG)
        .json(&json!({ "query": pulse_query(&watch) }))
        .send()
        .await
        .map_err(|err| tf("Network error: {error}", &[("error", &err.without_url().to_string())]))?;
    drop(token);
    let status = response.status();
    if !status.is_success() {
        return Err(tf("API error {code}", &[("code", &status.as_u16().to_string())]));
    }
    let body = net::read_capped(response, net::MAX_BODY).await?;
    let root: Value = serde_json::from_slice(&body).map_err(|_| t("Unexpected API response."))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0);
    let mut pulse = GitHubPulse::parse(&root, now).ok_or_else(|| t("Unexpected API response."))?;
    pulse.retain_watched(&watch);
    forge_into(&mut pulse).await;
    Ok(json!({ "pulse": pulse }))
}

// ── GitLab and Gitea ──────────────────────────────────────────────────────────
//
// Settings → Integrations → GitHub takes an address (`forge-url`, https only)
// and a token (`forge-token`, Credential Manager). Their merge requests,
// reviews and pipelines join the same pulse; the token goes to that host only.

struct Forge {
    base: reqwest::Url,
    token: String,
    gitlab: bool,
}

fn forge_setup() -> Option<Forge> {
    let mut base = net::keyed_url(&crate::secrets::get("forge-url")?).ok().filter(|u| u.scheme() == "https")?;
    let token = crate::secrets::get("forge-token").filter(|t| !t.trim().is_empty())?;
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    // debt: GitLab is told by its name in the address; any other host is taken for Gitea.
    let gitlab = base.host_str()?.to_ascii_lowercase().contains("gitlab");
    Some(Forge { base, token, gitlab })
}

fn forge_ci(raw: Option<&str>) -> CiState {
    match raw {
        Some("success") => CiState::Success,
        Some("failed") | Some("failure") | Some("error") | Some("canceled") | Some("cancelled") => CiState::Failure,
        Some("running") | Some("pending") | Some("created") | Some("preparing") | Some("scheduled")
        | Some("waiting_for_resource") => CiState::Pending,
        _ => CiState::Unknown,
    }
}

fn is_wip(title: &str) -> bool {
    let t = title.trim_start().to_ascii_lowercase();
    ["wip:", "[wip]", "draft:", "[draft]"].iter().any(|p| t.starts_with(p))
}

fn int_at(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn list(v: Option<Value>) -> Vec<Value> {
    v.and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

impl Forge {
    async fn get(&self, http: &reqwest::Client, path: &str, query: &[(&str, &str)]) -> Option<Value> {
        let mut url = self.base.join(path).ok()?;
        url.query_pairs_mut().extend_pairs(query);
        let request = http.get(url).header("User-Agent", notch_brand::SLUG);
        let request = if self.gitlab {
            request.header("PRIVATE-TOKEN", &self.token)
        } else {
            request.header("Authorization", format!("token {}", self.token))
        };
        let response = request.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        serde_json::from_slice(&net::read_capped(response, net::MAX_BODY).await.ok()?).ok()
    }

    fn pr(&self, repo: &str, number: i64, title: &str, url: &str, ci: CiState, sha: Option<String>, mine: bool) -> GitHubPr {
        GitHubPr {
            id: format!("{}:{repo}#{number}", self.base.host_str().unwrap_or("")),
            title: title.to_string(),
            url: url.to_string(),
            repo: repo.to_string(),
            number,
            is_draft: is_wip(title),
            ci,
            review: if mine { ReviewState::Unknown } else { ReviewState::Pending },
            head_sha: sha,
        }
    }

    /// (my requests, requests waiting for me, default branches), each at most a handful.
    async fn pulse(&self, http: &reqwest::Client) -> Option<(Vec<GitHubPr>, Vec<GitHubPr>, Vec<GitHubRepoCi>)> {
        let (mut mine, mut review, mut main) = (Vec::new(), Vec::new(), Vec::new());
        if self.gitlab {
            let me = self.get(http, "api/v4/user", &[]).await?;
            let me = str_at(&me, "username")?.to_string();
            let repo_of = |mr: &Value| {
                let full = mr.get("references").and_then(|r| str_at(r, "full")).unwrap_or("");
                full.split('!').next().unwrap_or("").to_string()
            };
            let own = list(self.get(http, "api/v4/merge_requests", &[("scope", "created_by_me"), ("state", "opened"), ("per_page", "10")]).await);
            for mr in &own {
                let (iid, project) = (int_at(mr, "iid"), int_at(mr, "project_id"));
                // debt: one request per merge request for its pipeline, ten at most.
                let full = self.get(http, &format!("api/v4/projects/{project}/merge_requests/{iid}"), &[]).await;
                let status = full.as_ref().and_then(|f| f.get("head_pipeline")).and_then(|p| str_at(p, "status"));
                mine.push(self.pr(&repo_of(mr), iid, str_at(mr, "title").unwrap_or(""), str_at(mr, "web_url").unwrap_or(""),
                    forge_ci(status), str_at(mr, "sha").map(str::to_string), true));
            }
            let asked = list(self.get(http, "api/v4/merge_requests", &[("scope", "all"), ("state", "opened"), ("reviewer_username", &me), ("per_page", "20")]).await);
            for mr in &asked {
                review.push(self.pr(&repo_of(mr), int_at(mr, "iid"), str_at(mr, "title").unwrap_or(""), str_at(mr, "web_url").unwrap_or(""),
                    CiState::Unknown, None, false));
            }
            let projects = list(self.get(http, "api/v4/projects", &[("membership", "true"), ("archived", "false"), ("order_by", "last_activity_at"), ("per_page", "5")]).await);
            for p in &projects {
                let (Some(repo), Some(url), Some(branch)) = (str_at(p, "path_with_namespace"), str_at(p, "web_url"), str_at(p, "default_branch")) else { continue };
                let pipes = list(self.get(http, &format!("api/v4/projects/{}/pipelines", int_at(p, "id")), &[("ref", branch), ("per_page", "1")]).await);
                let last = pipes.first();
                main.push(GitHubRepoCi {
                    repo: repo.to_string(), url: url.to_string(), branch: branch.to_string(),
                    ci: forge_ci(last.and_then(|l| str_at(l, "status"))),
                    head_sha: last.and_then(|l| str_at(l, "sha")).map(str::to_string),
                });
            }
        } else {
            let me = self.get(http, "api/v1/user", &[]).await?;
            let uid = int_at(&me, "id").to_string();
            let own = list(self.get(http, "api/v1/repos/issues/search", &[("type", "pulls"), ("state", "open"), ("created", "true"), ("limit", "10")]).await);
            for it in &own {
                let Some(repo) = it.get("repository").and_then(|r| str_at(r, "full_name")) else { continue };
                let number = int_at(it, "number");
                // debt: two requests per pull request for its commit status, ten at most.
                let full = self.get(http, &format!("api/v1/repos/{repo}/pulls/{number}"), &[]).await;
                let sha = full.as_ref().and_then(|f| f.get("head")).and_then(|h| str_at(h, "sha")).map(str::to_string);
                let status = match &sha {
                    Some(sha) => self.get(http, &format!("api/v1/repos/{repo}/commits/{sha}/status"), &[]).await,
                    None => None,
                };
                mine.push(self.pr(repo, number, str_at(it, "title").unwrap_or(""), str_at(it, "html_url").unwrap_or(""),
                    forge_ci(status.as_ref().and_then(|s| str_at(s, "state"))), sha, true));
            }
            let asked = list(self.get(http, "api/v1/repos/issues/search", &[("type", "pulls"), ("state", "open"), ("review_requested", "true"), ("limit", "20")]).await);
            for it in &asked {
                let Some(repo) = it.get("repository").and_then(|r| str_at(r, "full_name")) else { continue };
                review.push(self.pr(repo, int_at(it, "number"), str_at(it, "title").unwrap_or(""), str_at(it, "html_url").unwrap_or(""),
                    CiState::Unknown, None, false));
            }
            let repos = self.get(http, "api/v1/repos/search", &[("uid", &uid), ("sort", "updated"), ("order", "desc"), ("limit", "5")]).await;
            for r in repos.as_ref().and_then(|v| v.get("data")).and_then(Value::as_array).into_iter().flatten() {
                if r.get("archived").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                let (Some(repo), Some(url), Some(branch)) = (str_at(r, "full_name"), str_at(r, "html_url"), str_at(r, "default_branch")) else { continue };
                let status = self.get(http, &format!("api/v1/repos/{repo}/commits/{branch}/status"), &[]).await;
                main.push(GitHubRepoCi {
                    repo: repo.to_string(), url: url.to_string(), branch: branch.to_string(),
                    ci: forge_ci(status.as_ref().and_then(|s| str_at(s, "state"))),
                    head_sha: status.as_ref().and_then(|s| str_at(s, "sha")).map(str::to_string),
                });
            }
        }
        Some((mine, review, main))
    }
}

/// Adds the configured GitLab or Gitea host to the pulse. Silent when none is
/// set up or it does not answer: GitHub's part stays as it is.
pub async fn forge_into(pulse: &mut GitHubPulse) {
    if notch_core::paused() {
        return;
    }
    let Some(forge) = forge_setup() else { return };
    let Ok(http) = net::keyed_client(&forge.base, Duration::from_secs(15)) else { return };
    let Some((mine, review, main)) = forge.pulse(&http).await else { return };
    pulse.my_prs.extend(mine);
    pulse.to_review.extend(review);
    pulse.main_ci.extend(main);
    pulse.forge_host = forge.base.host_str().map(str::to_string);
}

// ── Tests (GitHubPulseTests.swift, GitHubActivityTests.swift) ────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_pulse() -> Value {
        json!({
          "data": {
            "viewer": {
              "login": "testuser",
              "pullRequests": { "nodes": [
                {
                  "number": 42, "title": "Add feature",
                  "url": "https://github.com/testuser/myrepo/pull/42",
                  "isDraft": false, "reviewDecision": "APPROVED",
                  "repository": {"nameWithOwner": "testuser/myrepo", "url": "https://github.com/testuser/myrepo"},
                  "commits": {"nodes": [{"commit": {"oid": "aaa", "statusCheckRollup": {"state": "SUCCESS"}}}]}
                },
                {
                  "number": 43, "title": "Fix bug",
                  "url": "https://github.com/testuser/myrepo/pull/43",
                  "isDraft": true, "reviewDecision": null,
                  "repository": {"nameWithOwner": "testuser/myrepo", "url": "https://github.com/testuser/myrepo"},
                  "commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]}
                }
              ]},
              "repositories": { "nodes": [
                {
                  "nameWithOwner": "testuser/myrepo", "url": "https://github.com/testuser/myrepo",
                  "isArchived": false,
                  "defaultBranchRef": {"name": "main", "target": {"oid": "bbb", "statusCheckRollup": {"state": "PENDING"}}}
                },
                {
                  "nameWithOwner": "testuser/archived", "url": "https://github.com/testuser/archived",
                  "isArchived": true,
                  "defaultBranchRef": {"name": "main", "target": {"statusCheckRollup": {"state": "SUCCESS"}}}
                }
              ]}
            },
            "reviewRequested": {
              "issueCount": 1,
              "nodes": [{
                "number": 7, "title": "Review this",
                "url": "https://github.com/other/repo/pull/7",
                "isDraft": false, "author": {"login": "otheruser"},
                "repository": {"nameWithOwner": "other/repo", "url": "https://github.com/other/repo"}
              }]
            }
          }
        })
    }

    fn empty_pulse() -> GitHubPulse {
        let root = json!({
          "data": {
            "viewer": {"login": "testuser", "pullRequests": {"nodes": []}, "repositories": {"nodes": []}},
            "reviewRequested": {"issueCount": 0, "nodes": []}
          }
        });
        GitHubPulse::parse(&root, 0).unwrap()
    }

    fn pr(id: &str, ci: CiState, sha: Option<&str>) -> GitHubPr {
        GitHubPr {
            id: id.into(),
            title: "T".into(),
            url: String::new(),
            repo: id.split('#').next().unwrap().into(),
            number: 1,
            is_draft: false,
            ci,
            review: ReviewState::Unknown,
            head_sha: sha.map(str::to_string),
        }
    }

    fn repo(name: &str, ci: CiState, sha: Option<&str>) -> GitHubRepoCi {
        GitHubRepoCi { repo: name.into(), url: String::new(), branch: "main".into(), ci, head_sha: sha.map(str::to_string) }
    }

    #[test]
    fn parses_a_full_pulse() {
        let p = GitHubPulse::parse(&valid_pulse(), 5).unwrap();
        assert_eq!(p.login, "testuser");
        assert_eq!(p.my_prs.len(), 2);
        assert_eq!(p.my_prs[0].id, "testuser/myrepo#42");
        assert_eq!(p.my_prs[0].ci, CiState::Success);
        assert_eq!(p.my_prs[0].review, ReviewState::Approved);
        assert_eq!(p.my_prs[0].head_sha.as_deref(), Some("aaa"));
        assert_eq!(p.my_prs[1].ci, CiState::Unknown, "null rollup");
        assert_eq!(p.my_prs[1].review, ReviewState::Unknown, "null review");
        assert!(p.my_prs[1].is_draft);
        assert_eq!(p.main_ci.len(), 1, "archived repo filtered out");
        assert_eq!(p.main_ci[0].repo, "testuser/myrepo");
        assert_eq!(p.main_ci[0].ci, CiState::Pending);
        assert_eq!(p.main_ci[0].branch, "main");
        assert_eq!(p.main_ci[0].head_sha.as_deref(), Some("bbb"));
        assert_eq!(p.to_review.len(), 1);
        assert_eq!(p.to_review[0].id, "other/repo#7");
        assert_eq!(p.to_review[0].review, ReviewState::Pending);
        assert!(p.has_pending(), "default branch pending");
        assert_eq!(p.fetched_at, 5);
    }

    #[test]
    fn parses_an_empty_pulse() {
        let p = empty_pulse();
        assert!(p.my_prs.is_empty() && p.main_ci.is_empty() && p.to_review.is_empty());
        assert!(!p.has_pending());
    }

    #[test]
    fn rejects_bad_pulse_data() {
        assert!(GitHubPulse::parse(&json!("garbage"), 0).is_none());
        assert!(GitHubPulse::parse(&json!({}), 0).is_none());
        assert!(GitHubPulse::parse(&json!({"data": null}), 0).is_none());
    }

    #[test]
    fn keeps_the_first_of_duplicate_prs() {
        let root = json!({
          "data": {
            "viewer": {
              "login": "testuser",
              "pullRequests": {"nodes": [
                {"number": 42, "title": "Add feature", "url": "u", "isDraft": false, "reviewDecision": "APPROVED",
                 "repository": {"nameWithOwner": "testuser/myrepo"},
                 "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS"}}}]}},
                {"number": 42, "title": "Duplicate entry", "url": "u", "isDraft": true, "reviewDecision": null,
                 "repository": {"nameWithOwner": "testuser/myrepo"},
                 "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}}
              ]},
              "repositories": {"nodes": []}
            },
            "reviewRequested": {"issueCount": 0, "nodes": []}
          }
        });
        let p = GitHubPulse::parse(&root, 0).unwrap();
        assert_eq!(p.my_prs.len(), 1);
        assert_eq!(p.my_prs[0].title, "Add feature");
        assert_eq!(p.my_prs[0].ci, CiState::Success);
    }

    #[test]
    fn maps_ci_states() {
        assert_eq!(CiState::from_github(None), CiState::Unknown);
        assert_eq!(CiState::from_github(Some("PENDING")), CiState::Pending);
        assert_eq!(CiState::from_github(Some("EXPECTED")), CiState::Pending);
        assert_eq!(CiState::from_github(Some("SUCCESS")), CiState::Success);
        assert_eq!(CiState::from_github(Some("FAILURE")), CiState::Failure);
        assert_eq!(CiState::from_github(Some("ERROR")), CiState::Failure);
        assert_eq!(CiState::from_github(Some("WAITING")), CiState::Unknown);
    }

    #[test]
    fn first_poll_is_silent() {
        let p = GitHubPulse::parse(&valid_pulse(), 0).unwrap();
        assert!(GitHubPulse::events(None, &p).is_empty());
    }

    #[test]
    fn pending_to_success_passes() {
        let mut old = empty_pulse();
        old.my_prs = vec![pr("r/p#1", CiState::Pending, None)];
        let mut new = old.clone();
        new.my_prs[0].ci = CiState::Success;
        assert_eq!(GitHubPulse::events(Some(&old), &new), vec![GitHubEvent::CiPassed { pr_id: "r/p#1".into() }]);
    }

    #[test]
    fn success_to_failure_fails() {
        let mut old = empty_pulse();
        old.my_prs = vec![pr("r/p#2", CiState::Success, None)];
        let mut new = old.clone();
        new.my_prs[0].ci = CiState::Failure;
        assert_eq!(GitHubPulse::events(Some(&old), &new), vec![GitHubEvent::CiFailed { pr_id: "r/p#2".into() }]);
    }

    #[test]
    fn default_branch_breaking_alerts() {
        let mut old = empty_pulse();
        old.main_ci = vec![repo("a/b", CiState::Success, None)];
        let mut new = old.clone();
        new.main_ci[0].ci = CiState::Failure;
        assert_eq!(GitHubPulse::events(Some(&old), &new), vec![GitHubEvent::MainFailed { repo: "a/b".into() }]);
    }

    #[test]
    fn default_branch_turning_green_is_quiet() {
        let mut old = empty_pulse();
        old.main_ci = vec![repo("a/b", CiState::Pending, Some("x"))];
        let mut new = old.clone();
        new.main_ci[0].ci = CiState::Success;
        assert!(GitHubPulse::events(Some(&old), &new).is_empty());
        new.main_ci[0].head_sha = Some("y".into());
        assert!(GitHubPulse::events(Some(&old), &new).is_empty());
    }

    #[test]
    fn new_review_request_alerts_once() {
        let mut review = pr("o/r#7", CiState::Unknown, None);
        review.review = ReviewState::Pending;
        let mut old = empty_pulse();
        let mut new = old.clone();
        new.to_review = vec![review.clone()];
        assert_eq!(
            GitHubPulse::events(Some(&old), &new),
            vec![GitHubEvent::ReviewRequested { pr_id: "o/r#7".into() }]
        );
        old.to_review = vec![review];
        assert!(GitHubPulse::events(Some(&old), &new).is_empty(), "already known");
    }

    #[test]
    fn duplicate_ids_in_old_fire_once() {
        let mut old = empty_pulse();
        let p = pr("r/p#1", CiState::Pending, None);
        old.my_prs = vec![p.clone(), p.clone()];
        let mut new = empty_pulse();
        let mut passed = p;
        passed.ci = CiState::Success;
        new.my_prs = vec![passed];
        let events = GitHubPulse::events(Some(&old), &new);
        assert_eq!(events.iter().filter(|e| matches!(e, GitHubEvent::CiPassed { .. })).count(), 1);
    }

    #[test]
    fn new_pr_already_green_passes() {
        let old = empty_pulse();
        let mut new = old.clone();
        new.my_prs = vec![pr("r/p#10", CiState::Success, Some("abc111"))];
        assert_eq!(GitHubPulse::events(Some(&old), &new), vec![GitHubEvent::CiPassed { pr_id: "r/p#10".into() }]);
    }

    #[test]
    fn new_pr_still_pending_is_quiet() {
        let old = empty_pulse();
        let mut new = old.clone();
        new.my_prs = vec![pr("r/p#11", CiState::Pending, Some("abc222"))];
        assert!(GitHubPulse::events(Some(&old), &new).is_empty());
    }

    #[test]
    fn new_commit_already_red_fails() {
        let mut old = empty_pulse();
        old.my_prs = vec![pr("r/p#12", CiState::Success, Some("sha-old"))];
        let mut new = old.clone();
        new.my_prs[0].ci = CiState::Failure;
        new.my_prs[0].head_sha = Some("sha-new".into());
        assert_eq!(GitHubPulse::events(Some(&old), &new), vec![GitHubEvent::CiFailed { pr_id: "r/p#12".into() }]);
    }

    #[test]
    fn new_commit_already_green_passes_even_from_green() {
        // A fast CI ran entirely between two polls: success → (new sha) success.
        let mut old = empty_pulse();
        old.my_prs = vec![pr("r/p#14", CiState::Success, Some("one"))];
        let mut new = old.clone();
        new.my_prs[0].head_sha = Some("two".into());
        assert_eq!(GitHubPulse::events(Some(&old), &new), vec![GitHubEvent::CiPassed { pr_id: "r/p#14".into() }]);
    }

    #[test]
    fn same_sha_success_stays_quiet() {
        let mut old = empty_pulse();
        old.my_prs = vec![pr("r/p#13", CiState::Success, Some("same"))];
        let new = old.clone();
        assert!(GitHubPulse::events(Some(&old), &new).is_empty());
    }

    #[test]
    fn new_commit_on_main_red_fails() {
        let mut old = empty_pulse();
        old.main_ci = vec![repo("a/b", CiState::Success, Some("sha-old"))];
        let mut new = old.clone();
        new.main_ci[0].ci = CiState::Failure;
        new.main_ci[0].head_sha = Some("sha-new".into());
        assert_eq!(GitHubPulse::events(Some(&old), &new), vec![GitHubEvent::MainFailed { repo: "a/b".into() }]);
    }

    #[test]
    fn staleness() {
        let now = 1_000_000;
        assert!(is_stale(None, now, 60));
        assert!(!is_stale(Some(now), now, 60));
        assert!(!is_stale(Some(now - 60_000), now, 60));
        assert!(is_stale(Some(now - 61_000), now, 60));
        assert!(!is_stale(Some(now + 5_000), now, 60), "clock skew is not stale");
    }

    #[test]
    fn notices_are_new_since_the_last_poll_and_watch_list_filters() {
        let old = GitHubPulse::parse(&valid_pulse(), 1).unwrap();
        // Same poll again: nothing. First poll (no old): nothing.
        assert!(alerts(Some(&old), &old, &GitHubPulse::events(Some(&old), &old)).is_empty());
        assert!(alerts(None, &old, &GitHubPulse::events(None, &old)).is_empty());

        // PR 42 turns red on a new commit, a second review is asked, 43 gets changes requested.
        let mut new = old.clone();
        new.my_prs[0].ci = CiState::Failure;
        new.my_prs[0].head_sha = Some("zzz".into());
        new.my_prs[1].review = ReviewState::ChangesRequested;
        let mut asked = new.to_review[0].clone();
        asked.id = "other/repo#8".into();
        new.to_review.push(asked);
        let got = alerts(Some(&old), &new, &GitHubPulse::events(Some(&old), &new));
        let kinds: Vec<_> = got.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, vec!["ciFailed", "review", "changes"]);
        assert_eq!(got[0].key, "ci:testuser/myrepo#42:zzz");

        // A watch list keeps only its repositories, and the query asks for them by name.
        let watch = vec!["other/repo".to_string()];
        new.retain_watched(&watch);
        assert!(new.my_prs.is_empty() && new.to_review.len() == 2);
        assert!(pulse_query(&watch).contains("r0: repository(owner: \"other\", name: \"repo\")"));
        assert!(pulse_query(&["a\"b/c".to_string()]).contains("r0:") == false);
    }

    #[test]
    fn events_serialise_for_the_island() {
        let v = serde_json::to_value(GitHubEvent::CiFailed { pr_id: "a/b#1".into() }).unwrap();
        assert_eq!(v, json!({"kind": "ciFailed", "prId": "a/b#1"}));
        let v = serde_json::to_value(GitHubEvent::MainFailed { repo: "a/b".into() }).unwrap();
        assert_eq!(v, json!({"kind": "mainFailed", "repo": "a/b"}));
        let p = serde_json::to_value(GitHubPulse::parse(&valid_pulse(), 7).unwrap()).unwrap();
        assert!(p.get("myPRs").is_some() && p.get("toReview").is_some() && p.get("mainCI").is_some());
        assert_eq!(p["myPRs"][0]["isDraft"], json!(false));
        assert_eq!(p["myPRs"][0]["headSha"], json!("aaa"));
        assert_eq!(p["myPRs"][0]["ci"], json!("success"));
        assert_eq!(p["myPRs"][0]["review"], json!("approved"));
        assert_eq!(p["fetchedAt"], json!(7));
    }

    // ── Activity ──────────────────────────────────────────────────────────────

    fn day(date: &str, count: i64, level: &str, weekday: i64) -> Value {
        json!({"date": date, "contributionCount": count, "contributionLevel": level, "weekday": weekday})
    }

    fn valid_activity() -> Value {
        json!({
          "data": { "viewer": { "login": "testuser", "contributionsCollection": { "contributionCalendar": {
            "totalContributions": 42,
            "weeks": [
              {"contributionDays": [
                day("2026-01-05", 0, "NONE", 0),
                day("2026-01-06", 1, "FIRST_QUARTILE", 1),
                day("2026-01-07", 4, "SECOND_QUARTILE", 2),
                day("2026-01-08", 8, "THIRD_QUARTILE", 3),
                day("2026-01-09", 12, "FOURTH_QUARTILE", 4),
                day("2026-01-10", 2, "FIRST_QUARTILE", 5),
                day("2026-01-11", 0, "NONE", 6)
              ]},
              {"contributionDays": [
                day("2026-01-12", 5, "SECOND_QUARTILE", 0),
                day("2026-01-13", 10, "THIRD_QUARTILE", 1)
              ]}
            ]
          }}}}
        })
    }

    #[test]
    fn parses_the_contribution_calendar() {
        let a = GitHubActivity::parse(&valid_activity(), 3).unwrap();
        assert_eq!(a.total, 42);
        assert_eq!(a.weeks.len(), 2);
        assert_eq!(a.weeks[0].len(), 7);
        assert_eq!(a.weeks[1].len(), 2, "incomplete current week");
        let levels: Vec<u8> = a.weeks[0][..5].iter().map(|d| d.level).collect();
        assert_eq!(levels, vec![0, 1, 2, 3, 4]);
        assert_eq!(a.weeks[0][0].date, "2026-01-05");
        assert_eq!(a.weeks[0][0].count, 0);
        assert_eq!(a.weeks[0][0].weekday, 0);
        assert_eq!(a.weeks[1][1].date, "2026-01-13");
        assert_eq!(a.weeks[1][1].count, 10);
        assert_eq!(a.fetched_at, 3);
    }

    #[test]
    fn unknown_contribution_level_is_zero() {
        let root = json!({"data": {"viewer": {"contributionsCollection": {"contributionCalendar": {
            "totalContributions": 1,
            "weeks": [{"contributionDays": [day("2026-03-01", 1, "EXTRA_SPECIAL", 0)]}]
        }}}}});
        assert_eq!(GitHubActivity::parse(&root, 0).unwrap().weeks[0][0].level, 0);
    }

    #[test]
    fn rejects_bad_activity_data() {
        assert!(GitHubActivity::parse(&json!("garbage"), 0).is_none());
        assert!(GitHubActivity::parse(&json!({}), 0).is_none());
    }

    #[test]
    fn counts_graphql_errors() {
        assert_eq!(graphql_error_count(&json!({"data": {}})), 0);
        assert_eq!(graphql_error_count(&json!({"errors": [{}, {}]})), 2);
    }
}
