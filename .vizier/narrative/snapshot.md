# Snapshot

- Product: CLI "annals" will compress git histories into readable logs spanning days, weeks, months, and years, with LLM summarization layered on top once commit data is available. Default scope is account-level so a user can see a cross-repo diary without manual per-repo setup.
- Current surface: Code is a stub that only prints "Hello, world!" (`src/main.rs`); no data retrieval or summarization exists yet.
- Active thread — gather commits for an account: Need GitHub API coverage that, given a username (or author identity), personal access token, and date range, collects all commits attributed to that author across accessible repos. Output should include commit message, repo name, timestamp, and URLs for later summarization; must paginate through the API and avoid missing commits across days/weeks/months. Errors for auth/rate limits should be surfaced clearly to guide next steps.
- Upcoming: After commit ingestion works, layer LLM-driven compression that groups commits by day/week/month/year and emits human-readable logs.
