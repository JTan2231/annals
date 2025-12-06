use annals::{
    CommitQueryInput, CommitRecord, EventSink, FetchOpts, ModelProvider, SummarizeOpts,
    SummaryOutputFormat, build_commit_query, fetch_commits, load_commits_from_file,
    pretty_format_summary, summarize_commits,
};
use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "annals",
    version,
    about = "Cross-repo commit ingestion for annals"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    FetchCommits(FetchCommitsArgs),
    Summarize(SummarizeArgs),
}

#[derive(Args, Debug, Clone)]
struct CommitQueryArgs {
    /// GitHub login to query commits for
    #[arg(long)]
    user: String,

    /// Inclusive lower bound (RFC3339, date like 2024-01-02, or relative like "30 days")
    #[arg(long)]
    since: String,

    /// Exclusive upper bound (RFC3339, date like 2024-01-02, or relative like "30 days")
    #[arg(long)]
    until: Option<String>,

    /// Personal Access Token (defaults to GITHUB_TOKEN if set)
    #[arg(long)]
    token: Option<String>,

    /// Optional repo filter in owner/name form
    #[arg(long)]
    repo: Option<String>,
}

impl CommitQueryArgs {
    fn into_input(self) -> CommitQueryInput {
        CommitQueryInput {
            user: self.user,
            since: self.since,
            until: self.until,
            token: self.token,
            repo: self.repo,
        }
    }
}

#[derive(Args, Debug)]
struct FetchCommitsArgs {
    #[command(flatten)]
    query: CommitQueryArgs,

    /// Output format for commits
    #[arg(long, value_enum, default_value = "human")]
    format: OutputFormat,

    /// Emit pagination and request progress
    #[arg(long)]
    verbose: bool,

    /// Stream JSON events describing progress to stderr
    #[arg(long)]
    events_json: bool,
}

#[derive(Args, Debug)]
struct SummarizeArgs {
    #[command(flatten)]
    query: CommitQueryArgs,

    #[command(flatten)]
    summarize: SummarizeCliOpts,

    /// Read commits from a JSON file instead of querying GitHub (same shape as fetch --format json)
    #[arg(long, value_name = "PATH")]
    input: Option<PathBuf>,

    /// Emit pagination and request progress
    #[arg(long)]
    verbose: bool,

    /// Stream JSON events describing progress to stderr
    #[arg(long)]
    events_json: bool,
}

#[derive(Args, Debug)]
struct SummarizeCliOpts {
    /// Override the LLM provider (defaults to OpenAI)
    #[arg(long, value_enum)]
    provider: Option<ModelProvider>,

    /// Override the LLM model (defaults to OpenAI gpt-5)
    #[arg(long)]
    model: Option<String>,

    /// Maximum characters allowed before compaction
    #[arg(long, default_value_t = annals::DEFAULT_MAX_CHARS)]
    max_chars: usize,

    /// Output format for the summary
    #[arg(long, value_enum, default_value = "human")]
    output: SummaryOutputFormat,

    /// Pretty print human-readable output (wrap to 80 columns with blank lines between sentences)
    #[arg(long)]
    pretty: bool,

    /// Custom base URL for the LLM API (primarily for testing)
    #[arg(long, hide = true)]
    base_url: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::FetchCommits(args) => run_fetch_commits(args).await?,
        Commands::Summarize(args) => run_summarize(args).await?,
    }
    Ok(())
}

async fn run_fetch_commits(args: FetchCommitsArgs) -> Result<()> {
    let event_sink = if args.events_json {
        EventSink::json_stderr()
    } else {
        EventSink::null()
    };
    let query = build_commit_query(&args.query.into_input())?;
    event_sink.emit(
        "normalized_query",
        json!({
            "since": query.since.to_rfc3339(),
            "until": query.until.to_rfc3339(),
            "repo_filter": query.repo_filter.as_ref().map(|r| json!({"owner": r.owner, "name": r.name})).unwrap_or(Value::Null),
            "token_present": query.token.is_some(),
            "user": query.user.clone(),
        }),
    );
    let commits = fetch_commits(
        &query,
        &FetchOpts {
            verbose: args.verbose,
            base_url: None,
            event_sink: event_sink.clone(),
        },
    )
    .await?;

    if commits.is_empty() {
        event_sink.emit("no_commits", json!({"count": 0}));
        println!("No commits found for the specified window.");
        return Ok(());
    }

    event_sink.emit("emit_commits_complete", json!({ "count": commits.len() }));

    match args.format {
        OutputFormat::Human => print_human(&commits),
        OutputFormat::Json => print_json(&commits)?,
    }

    Ok(())
}

async fn run_summarize(args: SummarizeArgs) -> Result<()> {
    let event_sink = if args.events_json {
        EventSink::json_stderr()
    } else {
        EventSink::null()
    };
    let commits = if let Some(ref input) = args.input {
        let loaded = load_commits_from_file(input)?;
        event_sink.emit(
            "summary_input",
            json!({
                "source": "file",
                "path": input.display().to_string(),
                "commit_count": loaded.len(),
            }),
        );
        loaded
    } else {
        let query = build_commit_query(&args.query.clone().into_input())?;
        let fetched = fetch_commits(
            &query,
            &FetchOpts {
                verbose: args.verbose,
                base_url: None,
                event_sink: event_sink.clone(),
            },
        )
        .await?;
        event_sink.emit(
            "summary_input",
            json!({
                "source": "github",
                "user": query.user,
                "commit_count": fetched.len(),
            }),
        );
        fetched
    };

    if commits.is_empty() {
        event_sink.emit("no_commits", json!({"count": 0}));
        println!("No commits found for the specified window.");
        return Ok(());
    }

    let outcome = summarize_commits(
        &commits,
        SummarizeOpts {
            provider: args.summarize.provider.map(|p| p.as_str().to_string()),
            model: args.summarize.model.clone(),
            max_chars: args.summarize.max_chars,
            verbose: args.verbose,
            base_url: args.summarize.base_url.clone(),
            event_sink: event_sink.clone(),
        },
    )
    .await?;

    if args.verbose {
        eprintln!(
            "LLM provider={} model={} context={} chars across {} chunk(s); filtered={} truncated_lines={}",
            outcome.provider,
            outcome.model,
            outcome.context_char_count,
            outcome.chunk_count,
            outcome.filtered_out,
            outcome.truncated_lines
        );
        if outcome.truncated {
            eprintln!(
                "Commit text compacted to respect {} character budget",
                args.summarize.max_chars
            );
        }
    }

    match args.summarize.output {
        SummaryOutputFormat::Human => {
            let mut summary = outcome.summary;
            if args.summarize.pretty {
                summary = pretty_format_summary(&summary);
            }
            println!("{summary}");
        }
        SummaryOutputFormat::Json => {
            let output = serde_json::to_string_pretty(&outcome)?;
            println!("{output}");
        }
    }

    Ok(())
}

fn print_human(commits: &[CommitRecord]) {
    for commit in commits {
        let short_sha: String = commit.sha.chars().take(7).collect();
        let first_line = commit
            .message
            .lines()
            .next()
            .unwrap_or("")
            .trim_end_matches('\n');
        println!(
            "{} {} {} - {} ({})",
            commit.timestamp.to_rfc3339(),
            commit.repo,
            short_sha,
            first_line,
            commit.url
        );
    }
}

fn print_json(commits: &[CommitRecord]) -> Result<()> {
    let output = serde_json::to_string_pretty(commits)?;
    println!("{output}");
    Ok(())
}
