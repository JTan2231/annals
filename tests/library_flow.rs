use annals::{
    build_commit_query, fetch_commits, summarize_commits, CommitQueryInput, FetchOpts, SummarizeOpts,
    DEFAULT_MODEL,
};
use httpmock::Method::GET;
use httpmock::MockServer;
use reqwest::Url;
use serde_json::json;
use wire::mock::{MockJsonResponse, MockLLMServer, MockResponse, MockRoute};

#[tokio::test]
async fn fetches_and_summarizes_programmatically() {
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "test");
    }

    let github = MockServer::start_async().await;

    let _repos = github
        .mock_async(|when, then| {
            when.method(GET)
                .path("/users/me/repos")
                .query_param("per_page", "100")
                .query_param("page", "1");
            then.status(200).json_body(json!([{
                "name": "demo",
                "full_name": "me/demo",
                "owner": { "login": "me" }
            }]));
        })
        .await;

    let _search = github
        .mock_async(|when, then| {
            when.method(GET).path("/search/commits");
            then.status(200).json_body(json!({
                "total_count": 1,
                "incomplete_results": false,
                "items": [{
                    "sha": "abc1234",
                    "html_url": "http://example/abc1234",
                    "commit": {
                        "message": "first commit",
                        "author": { "name": "me", "date": "2024-01-01T00:00:00Z" },
                        "committer": { "name": "ci", "date": "2024-01-01T00:00:00Z" }
                    },
                    "repository": { "full_name": "me/demo" }
                }]
            }));
        })
        .await;

    let query = build_commit_query(&CommitQueryInput {
        user: "me".into(),
        since: "2024-01-01".into(),
        until: Some("2024-01-03".into()),
        token: None,
        repo: None,
    })
    .expect("query builds");

    let commits = fetch_commits(
        &query,
        &FetchOpts {
            verbose: false,
            base_url: Some(Url::parse(&format!("{}/", github.base_url())).unwrap()),
        },
    )
    .await
    .expect("commits fetched");

    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].sha, "abc1234");

    let responses = vec![
        MockResponse::Json(MockJsonResponse::new(json!({
            "choices": [
                { "message": { "content": "partial summary" } }
            ]
        }))),
        MockResponse::Json(MockJsonResponse::new(json!({
            "choices": [
                { "message": { "content": "final story" } }
            ]
        }))),
    ];

    let llm_server =
        MockLLMServer::start(vec![MockRoute::new("/v1/chat/completions", responses)])
            .await
            .expect("starts mock llm");
    let llm_base = format!("http://{}", llm_server.address());

    let outcome = summarize_commits(
        &commits,
        SummarizeOpts {
            provider: Some("openai".into()),
            model: Some(DEFAULT_MODEL.to_string()),
            max_chars: 1_100,
            verbose: false,
            base_url: Some(llm_base),
        },
    )
    .await
    .expect("summarized");

    assert_eq!(outcome.summary, "final story");
    assert_eq!(outcome.total_commits, 1);

    llm_server.shutdown().await;
}
