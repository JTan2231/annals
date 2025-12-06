# Annals

`annals` is a CLI tool designed to ingest and summarize git commit activity across multiple repositories. It helps developers generate narrative reports of their work by fetching commit history from GitHub and processing it through Large Language Models (LLMs) to create cohesive summaries.

## Features

*   **Cross-Repo Ingestion**: Fetch commits from GitHub for a specific user across all repositories or specific ones.
*   **Flexible Date Ranges**: Support for RFC3339 timestamps, simple dates (e.g., `2024-01-01`), and relative durations (e.g., `30 days`).
*   **LLM Summarization**: transform raw commit logs into a narrative story of accomplishments using OpenAI, Anthropic, or Gemini.
*   **Smart Chunking**: Automatically splits large commit histories into chunks to fit within LLM context windows.
*   **JSON & Human Output**: Export raw data for processing or view pretty-printed summaries directly in the terminal.
*   **Streaming Events**: Optional JSON event stream on stderr for progress-aware scripts and dashboards.
*   **Library API**: Consume the same fetch and summarize flows directly from Rust without shelling out.

## Installation

To build and install from source, ensure you have Rust installed, then run:

```bash
cargo install --path .
```

## Configuration

`annals` relies on environment variables for authentication:

*   `GITHUB_TOKEN`: (Optional but recommended) GitHub Personal Access Token for higher rate limits and accessing private repositories.
*   `OPENAI_API_KEY`: For OpenAI (default).

## Usage

### Fetching Commits

Retrieve commits for a user within a specific timeframe.

```bash
# Fetch commits for user 'jdoe' for the last 7 days
annals fetch-commits --user jdoe --since "7 days"

# Fetch commits from a specific date
annals fetch-commits --user jdoe --since 2024-01-01 --until 2024-02-01

# Output as JSON (useful for piping to other tools or saving)
annals fetch-commits --user jdoe --since "30 days" --format json > commits.json

# Filter by a specific repository
annals fetch-commits --user jdoe --since "1 week" --repo owner/repo-name
```

### Summarizing Activity

Generate a narrative summary of your work. You can fetch and summarize in one go, or summarize pre-fetched JSON data.

```bash
# Fetch and summarize directly (defaults to OpenAI)
annals summarize --user jdoe --since "7 days"

# Summarize using a specific provider and model
annals summarize --user jdoe --since "7 days" --provider anthropic --model claude-3-opus

# Summarize from a local JSON file (previously fetched)
annals summarize --input commits.json

# Pretty print the output (wrapped text)
annals summarize --input commits.json --pretty
```

### Streaming JSON events

Both commands can stream machine-readable events to stderr without affecting stdout payloads:

```bash
annals fetch-commits --user jdoe --since "7 days" --events-json 2> events.log
annals summarize --input commits.json --events-json
```

Events are JSON objects with `event` and `payload` fields (e.g., repo discovery, search fallbacks, chunk summary start/finish, final summary stats).

## Embedding as a Library

`annals` exposes the same fetch/summarize pipeline as a crate so other tools can drive it programmatically.

```rust
use annals::{
    build_commit_query, fetch_commits, summarize_commits, CommitQueryInput, EventSink, FetchOpts,
    SummarizeOpts,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let query = build_commit_query(&CommitQueryInput {
        user: "jdoe".into(),
        since: "7 days".into(),
        until: None,
        token: std::env::var("GITHUB_TOKEN").ok(),
        repo: None,
    })?;

    let commits = fetch_commits(
        &query,
        &FetchOpts {
            event_sink: EventSink::json_stderr(),
            ..FetchOpts::default()
        },
    )
    .await?;
    let summary = summarize_commits(
        &commits,
        SummarizeOpts {
            event_sink: EventSink::json_stderr(),
            ..SummarizeOpts::default()
        },
    )
    .await?;
    println!("{}", summary.summary);
    Ok(())
}
```

Notes:

* `FetchOpts` accepts `base_url` for testing against mock GitHub servers; leave `None` for production.
* The API is new and may evolve; it mirrors the CLI flags so existing behaviors stay in sync.

## Development

To run tests:

```bash
./cicd.sh
```
