use crate::CommitRecord;
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, NaiveDate, Utc};
use clap::ValueEnum;
use serde::Serialize;
use std::env;
use wire::api::{API, Prompt};
use wire::config::ClientOptions;
use wire::types::{MessageBuilder, MessageType};

const PROMPT_PADDING: usize = 1_024;
pub const DEFAULT_MAX_CHARS: usize = 16_384;
pub const DEFAULT_MODEL: &str = "gpt-5";
const PRETTY_WRAP_WIDTH: usize = 80;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum SummaryOutputFormat {
    Human,
    Json,
}

#[derive(Debug, Clone)]
pub struct SummarizeOpts {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub max_chars: usize,
    pub verbose: bool,
    pub base_url: Option<String>,
}

impl Default for SummarizeOpts {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
            max_chars: DEFAULT_MAX_CHARS,
            verbose: false,
            base_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryOutcome {
    pub summary: String,
    pub provider: String,
    pub model: String,
    pub total_commits: usize,
    pub filtered_out: usize,
    pub included_commits: usize,
    pub context_char_count: usize,
    pub chunk_count: usize,
    pub truncated: bool,
    pub truncated_lines: usize,
}

pub async fn summarize_commits(
    commits: &[CommitRecord],
    opts: SummarizeOpts,
) -> Result<SummaryOutcome> {
    let total_commits = commits.len();
    let mut filtered: Vec<CommitRecord> = commits
        .iter()
        .filter(|c| !is_filtered(&c.message))
        .cloned()
        .collect();
    let filtered_out = total_commits.saturating_sub(filtered.len());

    if filtered.is_empty() {
        bail!("No commits to summarize after filtering VIZIER CONVERSATION entries");
    }

    filtered.sort_by_key(|c| c.timestamp);

    let formatted: Vec<String> = filtered.iter().map(format_commit_line).collect();

    let text_budget = effective_budget(opts.max_chars)?;
    let chunks = chunk_lines(&formatted, text_budget);
    let chunk_count = chunks.len();
    let context_char_count = chunks.iter().map(|c| c.len()).max().unwrap_or(0);
    let truncated_lines = 0usize;
    let truncated = chunk_count > 1;
    let timespan = describe_timespan(
        filtered.first().unwrap().timestamp,
        filtered.last().unwrap().timestamp,
    );

    let api = resolve_api(&opts)?;
    validate_api_key(&api)?;
    let client = build_client(&api, &opts)?;

    if opts.verbose {
        let (provider, model) = api.to_strings();
        eprintln!(
            "Summarizing {} commits ({} filtered) via {}:{} across {} chunk(s); largest chunk {} chars",
            filtered.len(),
            filtered_out,
            provider,
            model,
            chunk_count,
            context_char_count
        );
    }

    let mut partials = Vec::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        partials.push(summarize_chunk(client.as_ref(), &api, chunk, idx + 1, chunk_count).await?);
    }
    let summary = finalize_summary(client.as_ref(), &api, &partials.join("\n"), &timespan).await?;

    let (provider, model) = api.to_strings();
    Ok(SummaryOutcome {
        summary,
        provider,
        model,
        total_commits,
        filtered_out,
        included_commits: filtered.len(),
        context_char_count,
        chunk_count,
        truncated,
        truncated_lines,
    })
}

fn resolve_api(opts: &SummarizeOpts) -> Result<API> {
    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    if let Some(ref provider) = opts.provider {
        API::from_strings(provider, &model).map_err(|err| anyhow!(err))
    } else {
        API::from_model(&model).map_err(|err| anyhow!(err))
    }
}

fn validate_api_key(api: &API) -> Result<()> {
    let (env_var, label) = match api {
        API::OpenAI(_) => ("OPENAI_API_KEY", "OpenAI"),
        API::Anthropic(_) => ("ANTHROPIC_API_KEY", "Anthropic"),
        API::Gemini(_) => ("GEMINI_API_KEY", "Gemini"),
    };

    let value = env::var(env_var).unwrap_or_default();
    if value.trim().is_empty() {
        bail!("{label} credentials missing: set {env_var}");
    }
    Ok(())
}

fn build_client(api: &API, opts: &SummarizeOpts) -> Result<Box<dyn Prompt>> {
    if let Some(ref base_url) = opts.base_url {
        let options = ClientOptions::from_base_url(base_url)
            .with_context(|| format!("invalid LLM base URL {base_url}"))?;
        Ok(api.to_client_with_options(options))
    } else {
        Ok(api.to_client())
    }
}

fn summarize_prompt(chunk: &str, idx: usize, total: usize) -> (String, String) {
    let system_prompt = "You summarize git commit activity into a narrative of user accomplishments. Write as if telling a story of what was built or improved over time. Avoid meta-phrases like 'the commits show' or 'this batch includes'. Do not use bullet points or headings.";
    let content = format!(
        "Write one narrative sentence summarizing the key accomplishments from these commits (batch {}/{}) as part of a continuous story of progress. Keep it to a single line.\n\nCommits:\n{}",
        idx, total, chunk
    );
    (system_prompt.to_string(), content)
}

async fn summarize_chunk(
    client: &dyn Prompt,
    api: &API,
    chunk: &str,
    idx: usize,
    total: usize,
) -> Result<String> {
    let (system_prompt, content) = summarize_prompt(chunk, idx, total);
    prompt_model(client, api, &system_prompt, &content).await
}

async fn finalize_summary(
    client: &dyn Prompt,
    api: &API,
    partials: &str,
    timespan: &str,
) -> Result<String> {
    let (system_prompt, content) = final_prompt(partials, timespan);
    prompt_model(client, api, &system_prompt, &content).await
}

async fn prompt_model(
    client: &dyn Prompt,
    api: &API,
    system_prompt: &str,
    content: &str,
) -> Result<String> {
    let message = MessageBuilder::new(api.clone(), content.to_string())
        .message_type(MessageType::User)
        .with_system_prompt(system_prompt.to_string())
        .build();
    let response = client
        .prompt(system_prompt.to_string(), vec![message])
        .await
        .map_err(|err| anyhow!("LLM prompt failed: {}", err))?;
    Ok(response.content.trim().to_string())
}

fn effective_budget(max_chars: usize) -> Result<usize> {
    if max_chars <= PROMPT_PADDING {
        bail!(
            "max_chars of {} is too small; must exceed padding of {}",
            max_chars,
            PROMPT_PADDING
        );
    }
    Ok(max_chars - PROMPT_PADDING)
}

pub fn pretty_format_summary(summary: &str) -> String {
    let sentences = split_into_sentences(summary);
    let mut formatted = Vec::new();
    for sentence in sentences {
        let wrapped = wrap_text(&sentence, PRETTY_WRAP_WIDTH);
        if !wrapped.is_empty() {
            formatted.push(wrapped);
        }
    }
    formatted.join("\n\n")
}

fn format_commit_line(commit: &CommitRecord) -> String {
    let first_line = commit
        .message
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ");
    let short_sha: String = commit.sha.chars().take(7).collect();
    let line = format!(
        "{} | {} | {} | {}",
        commit.timestamp.to_rfc3339(),
        commit.repo,
        short_sha,
        first_line
    );
    line
}

fn split_into_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            while let Some(&next) = chars.peek() {
                if next.is_whitespace() {
                    current.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                sentences.push(trimmed.to_string());
            }
            current.clear();
        }
    }

    let trailing = current.trim();
    if !trailing.is_empty() {
        sentences.push(trailing.to_string());
    }

    sentences
}

fn wrap_text(text: &str, width: usize) -> String {
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let candidate_len = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };

        if candidate_len > width && !current.is_empty() {
            lines.push(current);
            current = word.to_string();
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    lines.join("\n")
}

fn chunk_lines(lines: &[String], budget: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in lines {
        if current.is_empty() {
            if line.len() <= budget {
                current.push_str(line);
            } else {
                // Single line exceeds budget; send it as its own chunk so the LLM can compact it.
                chunks.push(line.clone());
            }
            continue;
        }

        if current.len() + 1 + line.len() <= budget {
            current.push('\n');
            current.push_str(line);
        } else {
            chunks.push(current);
            if line.len() <= budget {
                current = line.clone();
            } else {
                chunks.push(line.clone());
                current = String::new();
            }
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

fn is_filtered(message: &str) -> bool {
    message.to_ascii_uppercase().contains("VIZIER CONVERSATION")
}

fn final_prompt(partials: &str, timespan: &str) -> (String, String) {
    let system_prompt = "You craft cohesive summaries of software development accomplishments. Write in a flowing narrative without bullet points or section headers.";
    let content = format!(
        "Bind this into a unified narrative unit describing the user's activities over the last {}. Keep it to a single paragraph with no list formatting.\n\nPartial summaries:\n{}",
        timespan, partials
    );
    (system_prompt.to_string(), content)
}

fn describe_timespan(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let start_date: NaiveDate = start.date_naive();
    let end_date: NaiveDate = end.date_naive();
    if start_date == end_date {
        start_date.to_string()
    } else {
        format!("{} to {}", start_date, end_date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use wire::mock::{MockJsonResponse, MockLLMServer, MockResponse, MockRoute};

    fn commit(repo: &str, sha: &str, message: &str) -> CommitRecord {
        CommitRecord {
            repo: repo.to_string(),
            sha: sha.to_string(),
            message: message.to_string(),
            timestamp: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            url: String::new(),
            author: None,
            committer: None,
        }
    }

    #[test]
    fn filters_conversation_commits() {
        let commits = vec![
            commit("demo/a", "a", "normal commit"),
            commit("demo/a", "b", "VIZIER conversation happened"),
        ];

        let filtered: Vec<_> = commits
            .iter()
            .filter(|c| !is_filtered(&c.message))
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].sha, "a");
    }

    #[test]
    fn formats_commit_lines_without_truncation() {
        let long_msg = "x".repeat(300);
        let line = format_commit_line(&commit("demo/b", "abc1234", &long_msg));
        assert!(line.contains("abc1234"));
        assert!(line.contains(&long_msg));
    }

    #[test]
    fn chunks_respect_budget() {
        let lines = vec!["a".repeat(50), "b".repeat(50), "c".repeat(50)];
        let chunks = chunk_lines(&lines, 60);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn pretty_format_wraps_and_separates_sentences() {
        let summary = "This is a very long sentence that should be wrapped across multiple lines to improve readability when rendered in a terminal environment. A second sentence should appear after a blank line to confirm separation.";
        let formatted = pretty_format_summary(summary);
        let parts: Vec<&str> = formatted.split("\n\n").collect();
        assert_eq!(parts.len(), 2);
        assert!(
            parts
                .iter()
                .all(|section| section.lines().all(|line| line.len() <= PRETTY_WRAP_WIDTH))
        );
    }

    #[test]
    fn splits_sentences_with_trailing_fragment() {
        let sentences = split_into_sentences("One sentence. Another without ending");
        assert_eq!(sentences.len(), 2);
        assert_eq!(sentences[0], "One sentence.");
        assert_eq!(sentences[1], "Another without ending");
    }

    #[tokio::test]
    async fn summarizes_with_mock_client() {
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test");
        }

        let responses = vec![
            MockResponse::Json(MockJsonResponse::new(json!({
                "choices": [
                    { "message": { "content": "partial one" } }
                ]
            }))),
            MockResponse::Json(MockJsonResponse::new(json!({
                "choices": [
                    { "message": { "content": "partial two" } }
                ]
            }))),
            MockResponse::Json(MockJsonResponse::new(json!({
                "choices": [
                    { "message": { "content": "partial three" } }
                ]
            }))),
            MockResponse::Json(MockJsonResponse::new(json!({
                "choices": [
                    { "message": { "content": "unified narrative" } }
                ]
            }))),
        ];

        let server = MockLLMServer::start(vec![MockRoute::new("/v1/chat/completions", responses)])
            .await
            .expect("starts server");

        let base_url = format!("http://{}", server.address());
        let commits = vec![
            commit("demo/a", "a", "first message"),
            commit("demo/a", "b", "second message"),
            commit("demo/a", "c", "third message"),
        ];

        let outcome = summarize_commits(
            &commits,
            SummarizeOpts {
                provider: Some("openai".to_string()),
                model: Some(DEFAULT_MODEL.to_string()),
                max_chars: 1_100,
                verbose: false,
                base_url: Some(base_url),
            },
        )
        .await
        .expect("summarizes");

        assert_eq!(outcome.summary, "unified narrative");
        assert_eq!(outcome.filtered_out, 0);
        assert_eq!(outcome.chunk_count, 3);
        assert!(outcome.truncated);

        server.shutdown().await;
    }
}
