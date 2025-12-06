use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, TimeZone, Utc};
use clap::ValueEnum;
use futures::stream::{self, StreamExt};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub mod summarize;

pub use summarize::{
    DEFAULT_MAX_CHARS, DEFAULT_MODEL, SummarizeOpts, SummaryOutcome, SummaryOutputFormat,
    pretty_format_summary, summarize_commits,
};

#[derive(Clone, Default)]
pub struct EventSink {
    emitter: Option<Arc<dyn EventEmitter>>,
}

impl EventSink {
    pub fn null() -> Self {
        Self { emitter: None }
    }

    pub fn json_stderr() -> Self {
        Self {
            emitter: Some(Arc::new(JsonStderrEmitter::default())),
        }
    }

    pub fn emit<T: Serialize>(&self, name: &str, payload: T) {
        let Some(emitter) = &self.emitter else {
            return;
        };
        match serde_json::to_value(payload) {
            Ok(value) => emitter.emit(name, value),
            Err(err) => {
                let mut stderr = io::stderr();
                let _ = writeln!(stderr, "failed to encode event {name}: {err}");
            }
        }
    }
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventSink")
            .field("enabled", &self.emitter.is_some())
            .finish()
    }
}

trait EventEmitter: Send + Sync {
    fn emit(&self, name: &str, payload: Value);
}

#[derive(Default)]
struct JsonStderrEmitter {
    lock: Mutex<()>,
}

impl EventEmitter for JsonStderrEmitter {
    fn emit(&self, name: &str, payload: Value) {
        let _guard = self.lock.lock().unwrap();
        let event = json!({ "event": name, "payload": payload });
        if let Ok(line) = serde_json::to_string(&event) {
            let mut stderr = io::stderr();
            let _ = writeln!(stderr, "{}", line);
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum ModelProvider {
    Openai,
    Anthropic,
    Gemini,
}

impl ModelProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelProvider::Openai => "openai",
            ModelProvider::Anthropic => "anthropic",
            ModelProvider::Gemini => "gemini",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Repo {
    pub name: String,
    pub full_name: String,
    pub owner: RepoOwner,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RepoOwner {
    pub login: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct GitHubCommit {
    sha: String,
    html_url: String,
    commit: CommitInfo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CommitInfo {
    message: String,
    author: Option<CommitUser>,
    committer: Option<CommitUser>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CommitUser {
    name: Option<String>,
    date: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SearchCommitsResponse {
    total_count: u64,
    incomplete_results: bool,
    items: Vec<SearchCommitItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SearchCommitItem {
    sha: String,
    html_url: String,
    commit: CommitInfo,
    repository: SearchCommitRepo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SearchCommitRepo {
    full_name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommitRecord {
    pub repo: String,
    pub sha: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
    pub url: String,
    pub author: Option<String>,
    pub committer: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CommitQueryInput {
    pub user: String,
    pub since: String,
    pub until: Option<String>,
    pub token: Option<String>,
    pub repo: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CommitQuery {
    pub user: String,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub token: Option<String>,
    pub repo_filter: Option<RepoFilter>,
}

#[derive(Debug, Clone, Default)]
pub struct FetchOpts {
    pub verbose: bool,
    pub base_url: Option<Url>,
    pub event_sink: EventSink,
}

#[derive(Clone)]
struct GitHubClient {
    client: Client,
    base_url: Url,
    token: Option<String>,
    verbose: bool,
    event_sink: EventSink,
}

pub fn build_commit_query(args: &CommitQueryInput) -> Result<CommitQuery> {
    let since = parse_datetime(&args.since)?;
    let until = match &args.until {
        Some(ts) => parse_datetime(ts)?,
        None => Utc::now(),
    };
    if since >= until {
        bail!("--since must be earlier than --until");
    }

    let token = args
        .token
        .clone()
        .or_else(|| env::var("GITHUB_TOKEN").ok())
        .filter(|t| !t.is_empty());
    let repo_filter = if let Some(ref repo) = args.repo {
        Some(RepoFilter::parse(repo)?)
    } else {
        None
    };

    Ok(CommitQuery {
        user: args.user.clone(),
        since,
        until,
        token,
        repo_filter,
    })
}

pub async fn fetch_commits(query: &CommitQuery, opts: &FetchOpts) -> Result<Vec<CommitRecord>> {
    let client = if let Some(base_url) = opts.base_url.clone() {
        GitHubClient::with_base_url(
            base_url,
            query.token.clone(),
            opts.verbose,
            opts.event_sink.clone(),
        )?
    } else {
        GitHubClient::new(query.token.clone(), opts.verbose, opts.event_sink.clone())?
    };
    let repos = client
        .list_repos(&query.user, query.repo_filter.as_ref())
        .await
        .context("failed to list repositories")?;

    if repos.is_empty() {
        bail!("No repositories found for user {}", query.user);
    }

    eprintln!("Discovered {} repositories to scan", repos.len());

    let commits = match client
        .search_commits(
            &query.user,
            query.since,
            query.until,
            query.repo_filter.as_ref(),
        )
        .await
    {
        Ok(commits) => commits,
        Err(err) => {
            if opts.verbose {
                eprintln!(
                    "Commit search unavailable ({}). Falling back to per-repo scanning.",
                    err
                );
            }
            collect_commits(&client, repos, &query.user, query.since, query.until).await?
        }
    };

    Ok(commits)
}

pub fn parse_datetime(input: &str) -> Result<DateTime<Utc>> {
    parse_datetime_with_now(input, Utc::now())
}

pub fn load_commits_from_file(path: &Path) -> Result<Vec<CommitRecord>> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read commit input file {}", path.display()))?;
    let mut commits: Vec<CommitRecord> = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse commit input file {}", path.display()))?;
    commits.sort_by_key(|c| c.timestamp);
    Ok(commits)
}

fn parse_datetime_with_now(input: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let trimmed = input.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Some(dt) = parse_relative(trimmed, now) {
        return Ok(dt);
    }
    if let Some(dt) = parse_date_only(trimmed) {
        return Ok(dt);
    }
    Err(anyhow!(
        "Invalid date/time '{}'. Use RFC3339 like 2024-01-02T15:04:05Z, a date like 2024-01-02 or 01/02/24, or a relative span like '30 days'",
        input
    ))
}

fn parse_relative(input: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mut parts = input.split_whitespace();
    let amount_str = parts.next()?;
    let unit = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let amount: i64 = amount_str.parse().ok()?;
    if amount < 0 {
        return None;
    }
    match unit.to_ascii_lowercase().as_str() {
        "day" | "days" | "d" => Some(now - Duration::days(amount)),
        "week" | "weeks" | "w" => Some(now - Duration::weeks(amount)),
        "month" | "months" | "mo" => {
            let months = u32::try_from(amount).ok()?;
            now.checked_sub_months(Months::new(months))
        }
        _ => None,
    }
}

fn parse_date_only(input: &str) -> Option<DateTime<Utc>> {
    const DATE_FORMATS: [(&str, bool); 8] = [
        ("%Y-%m-%d", false),
        ("%Y/%m/%d", false),
        ("%m-%d-%Y", false),
        ("%m/%d/%Y", false),
        ("%m-%d-%y", true),
        ("%m/%d/%y", true),
        ("%y-%m-%d", true),
        ("%y/%m/%d", true),
    ];

    for (format, two_digit_year) in DATE_FORMATS {
        if let Ok(date) = NaiveDate::parse_from_str(input, format) {
            if !two_digit_year && date.year() < 100 {
                // Probably a two-digit year that matched a %Y pattern; let other formats try.
                continue;
            }
            let adjusted = if two_digit_year && date.year() < 100 {
                date.with_year(2000 + date.year())?
            } else {
                date
            };
            let midnight = adjusted.and_hms_opt(0, 0, 0)?;
            return Some(Utc.from_utc_datetime(&midnight));
        }
    }

    None
}

async fn collect_commits(
    client: &GitHubClient,
    repos: Vec<Repo>,
    author: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CommitRecord>> {
    let concurrency = 5usize;
    let mut seen = HashSet::new();
    let mut aggregated: Vec<CommitRecord> = Vec::new();
    let mut stream = stream::iter(repos.into_iter().map(|repo| {
        let client = client.clone();
        let author = author.to_string();
        async move {
            client.emit(
                "repo_scan_start",
                json!({
                    "repo": repo.full_name.clone(),
                }),
            );
            let commits = client
                .fetch_commits_for_repo(&repo, &author, since, until)
                .await
                .with_context(|| format!("fetching commits for {}", repo.full_name))?;
            Result::<(Repo, Vec<CommitRecord>)>::Ok((repo, commits))
        }
    }))
    .buffer_unordered(concurrency);

    while let Some(result) = stream.next().await {
        let (repo, commits) = result?;
        client.emit(
            "repo_scan_finish",
            json!({
                "repo": repo.full_name.clone(),
                "commit_count": commits.len(),
            }),
        );
        for commit in commits {
            let key = (repo.full_name.clone(), commit.sha.clone());
            if seen.insert(key) {
                aggregated.push(commit);
            }
        }
    }

    aggregated.sort_by_key(|c| c.timestamp);
    client.emit(
        "dedup_complete",
        json!({
            "unique_commits": aggregated.len(),
        }),
    );
    Ok(aggregated)
}

#[derive(Debug, Clone)]
pub struct RepoFilter {
    pub owner: String,
    pub name: String,
}

impl RepoFilter {
    pub fn parse(raw: &str) -> Result<Self> {
        let mut parts = raw.split('/');
        let owner = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Repo filter must be owner/name"))?;
        let name = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Repo filter must be owner/name"))?;
        if parts.next().is_some() {
            bail!("Repo filter must be owner/name");
        }
        Ok(RepoFilter {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

impl GitHubClient {
    fn emit<T: Serialize>(&self, name: &str, payload: T) {
        self.event_sink.emit(name, payload);
    }

    fn new(token: Option<String>, verbose: bool, event_sink: EventSink) -> Result<Self> {
        let base_url = Url::parse("https://api.github.com/")?;
        Self::with_base_url(base_url, token, verbose, event_sink)
    }

    fn with_base_url(
        base_url: Url,
        token: Option<String>,
        verbose: bool,
        event_sink: EventSink,
    ) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "application/vnd.github+json, application/vnd.github.cloak-preview+json",
            ),
        );
        if let Some(ref token) = token {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .context("failed to encode Authorization header")?,
            );
        }

        let client = Client::builder()
            .default_headers(headers)
            .user_agent("annals-cli")
            .build()?;

        Ok(GitHubClient {
            client,
            base_url,
            token,
            verbose,
            event_sink,
        })
    }

    async fn list_repos(&self, login: &str, repo_filter: Option<&RepoFilter>) -> Result<Vec<Repo>> {
        let repos = if let Some(filter) = repo_filter {
            let repo = self.fetch_single_repo(&filter.owner, &filter.name).await?;
            vec![repo]
        } else if self.token.is_none() {
            self.list_public_repos(login).await?
        } else {
            let authenticated_login = self.authenticated_login().await?;
            if let Some(auth_login) = authenticated_login {
                if auth_login.eq_ignore_ascii_case(login) {
                    self.list_authenticated_repos().await?
                } else {
                    self.list_public_repos(login).await?
                }
            } else {
                self.list_public_repos(login).await?
            }
        };

        let mut seen = HashSet::new();
        let mut unique = Vec::new();
        for repo in repos {
            if seen.insert(repo.full_name.clone()) {
                unique.push(repo);
            }
        }
        self.emit(
            "discovered_repos",
            json!({
                "count": unique.len(),
                "repos": unique.iter().map(|r| r.full_name.clone()).collect::<Vec<_>>(),
            }),
        );

        Ok(unique)
    }

    async fn authenticated_login(&self) -> Result<Option<String>> {
        let url = self
            .base_url
            .join("user")
            .context("failed to construct /user URL")?;
        if self.verbose {
            eprintln!("GET {}", url);
        }
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            bail!("Authentication failed: GitHub rejected the provided token (401)");
        }
        if !status.is_success() {
            bail!(format_error(
                status,
                &headers,
                &body,
                "fetch authenticated user"
            ));
        }
        let user: AuthenticatedUser =
            serde_json::from_str(&body).context("failed to parse authenticated user response")?;
        Ok(Some(user.login))
    }

    async fn list_authenticated_repos(&self) -> Result<Vec<Repo>> {
        let params = vec![(
            "affiliation".to_string(),
            "owner,collaborator,organization_member".to_string(),
        )];
        self.paginated_get("user/repos", params, "list authenticated repos")
            .await
    }

    async fn list_public_repos(&self, login: &str) -> Result<Vec<Repo>> {
        let path = format!("users/{login}/repos");
        self.paginated_get(&path, Vec::new(), "list public repos")
            .await
    }

    async fn fetch_single_repo(&self, owner: &str, name: &str) -> Result<Repo> {
        let path = format!("repos/{owner}/{name}");
        let url = self
            .base_url
            .join(&path)
            .context("failed to construct repo URL")?;
        if self.verbose {
            eprintln!("GET {}", url);
        }
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(format_error(
                status,
                &headers,
                &body,
                &format!("fetch repo {owner}/{name}")
            ));
        }
        let repo: Repo = serde_json::from_str(&body).context("failed to parse repo response")?;
        Ok(repo)
    }

    async fn paginated_get(
        &self,
        path: &str,
        base_params: Vec<(String, String)>,
        context_label: &str,
    ) -> Result<Vec<Repo>> {
        let mut all = Vec::new();
        let mut page = 1usize;
        loop {
            let url = self.page_url(path, &base_params, page)?;
            if self.verbose {
                eprintln!("GET {} (page {})", url, page);
            }
            let resp = self.client.get(url).send().await?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let has_next = has_next_link(&headers);
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!(format_error(
                    status,
                    &headers,
                    &body,
                    &format!("{context_label} page {page}")
                ));
            }
            let mut repos: Vec<Repo> =
                serde_json::from_str(&body).context("failed to parse repos response")?;
            let page_len = repos.len();
            self.emit(
                "repos_page",
                json!({
                    "page": page,
                    "repos_found": page_len,
                    "has_next": has_next,
                }),
            );
            all.append(&mut repos);
            if !has_next || page_len == 0 {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    fn page_url(&self, path: &str, base_params: &[(String, String)], page: usize) -> Result<Url> {
        let mut url = self.base_url.join(path)?;
        {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in base_params {
                pairs.append_pair(k, v);
            }
            pairs.append_pair("per_page", "100");
            pairs.append_pair("page", &page.to_string());
        }
        Ok(url)
    }

    async fn fetch_commits_for_repo(
        &self,
        repo: &Repo,
        author: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CommitRecord>> {
        let path = format!("repos/{}/{}", repo.owner.login, repo.name);
        let mut params = Vec::with_capacity(3);
        params.push(("author".to_string(), author.to_string()));
        params.push(("since".to_string(), since.to_rfc3339()));
        params.push(("until".to_string(), until.to_rfc3339()));

        let mut all: Vec<CommitRecord> = Vec::new();
        let mut page = 1usize;
        loop {
            let url = self.page_url(&format!("{path}/commits"), &params, page)?;
            if self.verbose {
                eprintln!("GET {} (repo {} page {})", url, repo.full_name, page);
            }
            let resp = self.client.get(url).send().await?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let has_next = has_next_link(&headers);
            let body = resp.text().await.unwrap_or_default();
            if status == StatusCode::NOT_FOUND {
                bail!(format!("Repo {} not found or inaccessible", repo.full_name));
            }
            if !status.is_success() {
                bail!(format_error(
                    status,
                    &headers,
                    &body,
                    &format!("fetch commits for {} page {}", repo.full_name, page)
                ));
            }
            let commits: Vec<GitHubCommit> =
                serde_json::from_str(&body).context("failed to parse commits response")?;
            let page_len = commits.len();
            self.emit(
                "repo_commits_page",
                json!({
                    "repo": repo.full_name.clone(),
                    "page": page,
                    "items": page_len,
                    "has_next": has_next,
                }),
            );
            for commit in commits {
                let timestamp = commit
                    .commit
                    .author
                    .as_ref()
                    .map(|a| a.date)
                    .or_else(|| commit.commit.committer.as_ref().map(|c| c.date))
                    .unwrap_or_else(Utc::now);
                all.push(CommitRecord {
                    repo: repo.full_name.clone(),
                    sha: commit.sha,
                    message: commit.commit.message,
                    timestamp,
                    url: commit.html_url,
                    author: commit.commit.author.and_then(|a| a.name),
                    committer: commit.commit.committer.and_then(|c| c.name),
                });
            }
            if !has_next || page_len == 0 {
                break;
            }
            page += 1;
        }

        Ok(all)
    }

    async fn search_commits(
        &self,
        author: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        repo_filter: Option<&RepoFilter>,
    ) -> Result<Vec<CommitRecord>> {
        let mut query_parts = vec![
            format!("author:{author}"),
            format!(
                "committer-date:{}..{}",
                since.to_rfc3339(),
                until.to_rfc3339()
            ),
        ];
        if let Some(filter) = repo_filter {
            query_parts.push(format!("repo:{}/{}", filter.owner, filter.name));
        }
        let query = query_parts.join(" ");

        let mut all = Vec::new();
        let mut seen = HashSet::new();
        let mut page = 1usize;

        loop {
            let mut url = self.base_url.join("search/commits")?;
            {
                let mut pairs = url.query_pairs_mut();
                pairs.append_pair("q", &query);
                pairs.append_pair("sort", "committer-date");
                pairs.append_pair("order", "desc");
                pairs.append_pair("per_page", "100");
                pairs.append_pair("page", &page.to_string());
            }
            if self.verbose {
                eprintln!("GET {} (commit search page {})", url, page);
            }

            let resp = self.client.get(url).send().await.map_err(|err| {
                self.emit(
                    "falling_back_to_per_repo",
                    json!({
                        "reason": "search_request_failed",
                        "page": page,
                        "error": err.to_string(),
                    }),
                );
                err
            })?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let has_next = has_next_link(&headers);
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                self.emit(
                    "falling_back_to_per_repo",
                    json!({
                        "reason": "search_http_error",
                        "status": status.as_u16(),
                        "page": page,
                    }),
                );
                bail!(format_error(
                    status,
                    &headers,
                    &body,
                    &format!("search commits page {}", page)
                ));
            }

            let payload: SearchCommitsResponse = serde_json::from_str(&body).map_err(|err| {
                self.emit(
                    "falling_back_to_per_repo",
                    json!({
                        "reason": "search_decode_failed",
                        "page": page,
                        "error": err.to_string(),
                    }),
                );
                anyhow!("failed to parse commit search response: {}", err)
            })?;
            let page_len = payload.items.len();
            self.emit(
                "search_page",
                json!({
                    "page": page,
                    "items": page_len,
                    "total_count": payload.total_count,
                    "incomplete_results": payload.incomplete_results,
                }),
            );
            if payload.total_count > 1000 || payload.incomplete_results {
                self.emit(
                    "falling_back_to_per_repo",
                    json!({
                        "reason": "search_incomplete",
                        "page": page,
                        "total_count": payload.total_count,
                        "incomplete_results": payload.incomplete_results,
                    }),
                );
                bail!(format!(
                    "commit search incomplete (total_count={}, incomplete_results={})",
                    payload.total_count, payload.incomplete_results
                ));
            }

            for item in payload.items {
                let timestamp = item
                    .commit
                    .author
                    .as_ref()
                    .map(|a| a.date)
                    .or_else(|| item.commit.committer.as_ref().map(|c| c.date))
                    .unwrap_or_else(Utc::now);
                let repo_name = item.repository.full_name;
                let key = (repo_name.clone(), item.sha.clone());
                if seen.insert(key) {
                    all.push(CommitRecord {
                        repo: repo_name,
                        sha: item.sha,
                        message: item.commit.message,
                        timestamp,
                        url: item.html_url,
                        author: item.commit.author.and_then(|a| a.name),
                        committer: item.commit.committer.and_then(|c| c.name),
                    });
                }
            }

            if !has_next || page_len == 0 {
                break;
            }
            page += 1;
        }

        all.sort_by_key(|c| c.timestamp);
        self.emit(
            "dedup_complete",
            json!({
                "unique_commits": all.len(),
            }),
        );
        Ok(all)
    }
}

fn has_next_link(headers: &HeaderMap) -> bool {
    headers
        .get("link")
        .and_then(|value| value.to_str().ok())
        .map(|links| links.split(',').any(|part| part.contains("rel=\"next\"")))
        .unwrap_or(false)
}

fn format_error(status: StatusCode, headers: &HeaderMap, body: &str, context: &str) -> String {
    if status == StatusCode::UNAUTHORIZED {
        return format!("{context} failed: authentication rejected (401)");
    }
    if status == StatusCode::FORBIDDEN {
        if let Some(rate) = format_rate_limit(headers) {
            return format!("{context} hit GitHub rate limit: {rate}");
        }
        return format!("{context} forbidden (403) - token may lack scopes or repo access");
    }
    format!("{context} failed with {} - {}", status, body)
}

fn format_rate_limit(headers: &HeaderMap) -> Option<String> {
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())?;
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());

    if let Some(reset) = reset {
        if let Some(reset_time) = DateTime::from_timestamp(reset, 0) {
            return Some(format!(
                "remaining={}, resets at {}",
                remaining,
                reset_time.to_rfc3339()
            ));
        }
    }

    Some(format!("remaining={}", remaining))
}

#[derive(Debug, Deserialize)]
struct AuthenticatedUser {
    login: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::GET;
    use httpmock::{Mock, MockServer};
    use std::fs;

    fn client_for(server: &MockServer) -> GitHubClient {
        let base = Url::parse(&format!("{}/", server.base_url())).unwrap();
        GitHubClient::with_base_url(base, None, false, EventSink::null()).unwrap()
    }

    #[tokio::test]
    async fn repo_filter_parses() {
        let filter = RepoFilter::parse("owner/name").unwrap();
        assert_eq!(filter.owner, "owner");
        assert_eq!(filter.name, "name");
    }

    #[test]
    fn parses_date_only_inputs() {
        let now = Utc.with_ymd_and_hms(2024, 2, 1, 12, 0, 0).unwrap();
        let parsed = parse_datetime_with_now("2024-02-03", now).unwrap();
        let expected = Utc.with_ymd_and_hms(2024, 2, 3, 0, 0, 0).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parses_two_digit_years() {
        let now = Utc.with_ymd_and_hms(2024, 2, 1, 12, 0, 0).unwrap();
        let parsed = parse_datetime_with_now("12-01-25", now).unwrap();
        let expected = Utc.with_ymd_and_hms(2025, 12, 1, 0, 0, 0).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parses_relative_durations() {
        let now = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let parsed_days = parse_datetime_with_now("3 days", now).unwrap();
        let expected_days = Utc.with_ymd_and_hms(2024, 5, 29, 12, 0, 0).unwrap();
        assert_eq!(parsed_days, expected_days);

        let parsed_months = parse_datetime_with_now("2 months", now).unwrap();
        let expected_months = Utc.with_ymd_and_hms(2024, 4, 1, 12, 0, 0).unwrap();
        assert_eq!(parsed_months, expected_months);
    }

    #[tokio::test]
    async fn paginates_repos() {
        let server = MockServer::start_async().await;
        let first = Repo {
            name: "one".into(),
            full_name: "me/one".into(),
            owner: RepoOwner { login: "me".into() },
        };
        let second = Repo {
            name: "two".into(),
            full_name: "me/two".into(),
            owner: RepoOwner { login: "me".into() },
        };
        let _m1 = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/users/me/repos")
                    .query_param("per_page", "100")
                    .query_param("page", "1");
                then.status(200)
                    .header(
                        "link",
                        "</users/me/repos?page=2>; rel=\"next\", </users/me/repos?page=2>; rel=\"last\"",
                    )
                    .json_body_obj(&vec![&first]);
            })
            .await;

        let _m2 = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/users/me/repos")
                    .query_param("per_page", "100")
                    .query_param("page", "2");
                then.status(200).json_body_obj(&vec![&second]);
            })
            .await;

        let client = client_for(&server);
        let repos = client.list_public_repos("me").await.unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].full_name, "me/one");
        assert_eq!(repos[1].full_name, "me/two");
    }

    #[tokio::test]
    async fn fetches_commits_with_pagination() {
        let server = MockServer::start_async().await;
        let client = client_for(&server);
        let repo = Repo {
            name: "demo".into(),
            full_name: "me/demo".into(),
            owner: RepoOwner { login: "me".into() },
        };

        let commit_one = GitHubCommit {
            sha: "abc1234".into(),
            html_url: "http://example/abc1234".into(),
            commit: CommitInfo {
                message: "First".into(),
                author: Some(CommitUser {
                    name: Some("me".into()),
                    date: DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                }),
                committer: None,
            },
        };
        let commit_two = GitHubCommit {
            sha: "def5678".into(),
            html_url: "http://example/def5678".into(),
            commit: CommitInfo {
                message: "Second".into(),
                author: None,
                committer: Some(CommitUser {
                    name: Some("ci".into()),
                    date: DateTime::parse_from_rfc3339("2024-01-02T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                }),
            },
        };

        let _m1: Mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/repos/me/demo/commits")
                    .query_param("author", "me")
                    .query_param("since", "2024-01-01T00:00:00+00:00")
                    .query_param("until", "2024-01-03T00:00:00+00:00")
                    .query_param("per_page", "100")
                    .query_param("page", "1");
                then.status(200).header(
                    "link",
                    "</repos/me/demo/commits?page=2>; rel=\"next\", </repos/me/demo/commits?page=2>; rel=\"last\"",
                )
                .json_body_obj(&vec![&commit_one]);
            })
            .await;

        let _m2: Mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/repos/me/demo/commits")
                    .query_param("author", "me")
                    .query_param("since", "2024-01-01T00:00:00+00:00")
                    .query_param("until", "2024-01-03T00:00:00+00:00")
                    .query_param("per_page", "100")
                    .query_param("page", "2");
                then.status(200).json_body_obj(&vec![&commit_two]);
            })
            .await;

        let commits = client
            .fetch_commits_for_repo(
                &repo,
                "me",
                DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                DateTime::parse_from_rfc3339("2024-01-03T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )
            .await
            .unwrap();

        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "abc1234");
        assert_eq!(commits[1].sha, "def5678");
    }

    #[tokio::test]
    async fn fetches_commits_via_search() {
        let server = MockServer::start_async().await;
        let client = client_for(&server);

        let response = SearchCommitsResponse {
            total_count: 2,
            incomplete_results: false,
            items: vec![
                SearchCommitItem {
                    sha: "abc1234".into(),
                    html_url: "http://example/abc1234".into(),
                    repository: SearchCommitRepo {
                        full_name: "me/demo".into(),
                    },
                    commit: CommitInfo {
                        message: "First".into(),
                        author: Some(CommitUser {
                            name: Some("me".into()),
                            date: DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                                .unwrap()
                                .with_timezone(&Utc),
                        }),
                        committer: None,
                    },
                },
                SearchCommitItem {
                    sha: "def5678".into(),
                    html_url: "http://example/def5678".into(),
                    repository: SearchCommitRepo {
                        full_name: "me/demo".into(),
                    },
                    commit: CommitInfo {
                        message: "Second".into(),
                        author: None,
                        committer: Some(CommitUser {
                            name: Some("ci".into()),
                            date: DateTime::parse_from_rfc3339("2024-01-02T00:00:00Z")
                                .unwrap()
                                .with_timezone(&Utc),
                        }),
                    },
                },
            ],
        };

        let _m1: Mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/search/commits");
                then.status(200).json_body_obj(&response);
            })
            .await;

        let commits = client
            .search_commits(
                "me",
                DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                DateTime::parse_from_rfc3339("2024-01-03T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                None,
            )
            .await
            .unwrap();

        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "abc1234");
        assert_eq!(commits[1].repo, "me/demo");
    }

    #[test]
    fn loads_commits_from_file() {
        let path = std::env::temp_dir().join("annals_commits_input.json");
        let commit = CommitRecord {
            repo: "demo/repo".into(),
            sha: "abc1234".into(),
            message: "sample commit".into(),
            timestamp: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            url: "http://example/abc1234".into(),
            author: Some("me".into()),
            committer: None,
        };
        let body = serde_json::to_string(&vec![commit]).unwrap();
        fs::write(&path, body).unwrap();

        let loaded = load_commits_from_file(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].sha, "abc1234");
        assert_eq!(loaded[0].repo, "demo/repo");
    }
}
