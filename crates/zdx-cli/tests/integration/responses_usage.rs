//! A Responses API turn must record the provider's usage exactly once.
//!
//! Every Responses provider (`openai`, `openai-codex`, `meta`, `muse-code`,
//! `xai`, `grok-build`) shares one event mapper. When its terminal events
//! reached the engine out of order, each request was stored at exactly twice
//! the usage the API reported, which also doubled `zdx stats` and cost.

use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use crate::fixtures::sse_response;

const RESPONSES_SSE: &str = concat!(
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,",
    "\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",",
    "\"output_index\":0,\"content_index\":0,\"delta\":\"pong\"}\n\n",
    "event: response.output_item.done\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
    "\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",",
    "\"content\":[{\"type\":\"output_text\",\"text\":\"pong\"}]}}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",",
    "\"status\":\"completed\",\"usage\":{\"input_tokens\":13,",
    "\"input_tokens_details\":{\"cached_tokens\":4},\"output_tokens\":262,",
    "\"total_tokens\":275}}}\n\n",
);

fn can_bind_localhost() -> bool {
    std::net::TcpListener::bind("127.0.0.1:0").is_ok()
}

#[tokio::test]
async fn responses_turn_records_provider_usage_once() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let work_dir = TempDir::new().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(sse_response(RESPONSES_SSE))
        .expect(1)
        .mount(&mock_server)
        .await;

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .env("OPENAI_API_KEY", "test-api-key")
        .env("OPENAI_BASE_URL", format!("{}/v1", mock_server.uri()))
        .args([
            "--root",
            work_dir.path().to_str().unwrap(),
            "exec",
            "--no-tools",
            "-m",
            "openai:test-model",
            "-p",
            "ping",
        ])
        .assert()
        .success();

    let threads_dir = zdx_home.path().join("threads");
    let usage_rows: Vec<Value> = std::fs::read_dir(&threads_dir)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .flat_map(|contents| {
            contents
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .filter(|event| event["type"] == "usage")
        .collect();

    assert_eq!(
        usage_rows.len(),
        1,
        "expected one usage row: {usage_rows:?}"
    );
    let usage = &usage_rows[0];
    assert_eq!(usage["provider"], "openai");
    assert_eq!(usage["input_tokens"], 9);
    assert_eq!(usage["cache_read_tokens"], 4);
    assert_eq!(usage["cache_write_tokens"], 0);
    assert_eq!(usage["output_tokens"], 262);
}
