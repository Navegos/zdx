//! Tests for `zdx exec --subagent <name>`.

use std::fs;
use std::sync::{Arc, Mutex};

use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request};

use crate::fixtures::{MOCK_MODEL, sse_response, text_sse};

fn can_bind_localhost() -> bool {
    std::net::TcpListener::bind("127.0.0.1:0").is_ok()
}

fn write_subagent(root: &std::path::Path) {
    let dir = root.join(".zdx").join("subagents");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("tester.md"),
        "---\ndescription: Test subagent\ntools:\n  - read\n---\nYou are the tester subagent. SENTINEL-PROMPT-42.\n",
    )
    .unwrap();
}

#[tokio::test]
async fn test_exec_subagent_applies_prompt_and_tools() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    write_subagent(root.path());

    let mock_server = MockServer::start().await;
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = Arc::clone(&captured);
    let body = text_sse("done");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            *captured_clone.lock().unwrap() = req.body_json().ok();
            sse_response(&body)
        })
        .expect(1)
        .mount(&mock_server)
        .await;

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .env("ANTHROPIC_API_KEY", "test-api-key")
        .env("ANTHROPIC_BASE_URL", mock_server.uri())
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--no-thread",
            "exec",
            "-m",
            MOCK_MODEL,
            "--subagent",
            "tester",
            "-p",
            "hello",
        ])
        .assert()
        .success();

    let request = captured.lock().unwrap().clone().expect("captured request");

    let system = request["system"].to_string();
    assert!(
        system.contains("SENTINEL-PROMPT-42"),
        "subagent prompt body missing from system prompt: {system}"
    );

    let tools = request["tools"].as_array().expect("tools array");
    let names: Vec<String> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_lowercase())
        .collect();
    assert_eq!(names, vec!["read".to_string()], "tools not restricted");
}

#[tokio::test]
async fn test_exec_unknown_subagent_fails() {
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .env("ANTHROPIC_API_KEY", "test-api-key")
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--no-thread",
            "exec",
            "-m",
            MOCK_MODEL,
            "--subagent",
            "nope",
            "-p",
            "hello",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nope"));
}

/// Runs `zdx exec --subagent <name>` once and returns the tools it offered.
async fn subagent_tools(definition: &str) -> Vec<serde_json::Value> {
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let dir = root.path().join(".zdx").join("subagents");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("shell-helper.md"), definition).unwrap();

    let mock_server = MockServer::start().await;
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = Arc::clone(&captured);
    let body = text_sse("done");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            *captured_clone.lock().unwrap() = req.body_json().ok();
            sse_response(&body)
        })
        .expect(1)
        .mount(&mock_server)
        .await;

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .env("ANTHROPIC_API_KEY", "test-api-key")
        .env("ANTHROPIC_BASE_URL", mock_server.uri())
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--no-thread",
            "exec",
            "-m",
            MOCK_MODEL,
            "--subagent",
            "shell-helper",
            "-p",
            "hello",
        ])
        .assert()
        .success();

    let request = captured.lock().unwrap().clone().expect("captured request");
    request["tools"].as_array().cloned().expect("tools array")
}

fn sorted_names(tools: &[serde_json::Value]) -> Vec<String> {
    let mut names: Vec<String> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_lowercase())
        .collect();
    names.sort();
    names
}

fn bash_has_background_property(tools: &[serde_json::Value]) -> bool {
    tools
        .iter()
        .find(|tool| {
            tool["name"]
                .as_str()
                .unwrap_or_default()
                .eq_ignore_ascii_case("bash")
        })
        .expect("bash offered")["input_schema"]["properties"]
        .get("background")
        .is_some()
}

/// A subagent that does not opt in gets `bash` without background execution:
/// no management tools and no `background` property.
#[tokio::test]
async fn test_exec_subagent_without_opt_in_has_no_background_execution() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let tools = subagent_tools(
        "---\ndescription: Shell helper\ntools:\n  - bash\n  - read\n---\nYou run commands.\n",
    )
    .await;

    assert_eq!(sorted_names(&tools), vec!["bash", "read"]);
    assert!(!bash_has_background_property(&tools));
}

/// A subagent that opts in gets background execution together with the
/// tools that read and stop its jobs, even when its `tools` list omits them.
#[tokio::test]
async fn test_exec_subagent_opt_in_gets_background_management_tools() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let tools = subagent_tools(
        "---\ndescription: Shell helper\nbackground_commands: true\ntools:\n  - bash\n---\nYou run commands.\n",
    )
    .await;

    assert_eq!(
        sorted_names(&tools),
        vec!["background_kill", "background_output", "bash"]
    );
    assert!(bash_has_background_property(&tools));
}

const CHILD_MARKER: &str = "CHILD-TASK-bg-policy";

/// The incident path end to end: a parent `exec` run (background execution
/// on) delegates through `invoke_subagent`, and the child of a subagent that
/// does not opt in runs without background execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_invoke_subagent_child_inherits_definition_background_policy() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let dir = root.path().join(".zdx").join("subagents");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("shell-helper.md"),
        "---\ndescription: Shell helper\ntools:\n  - bash\n  - read\n---\nYou run commands.\n",
    )
    .unwrap();

    let invoke_input = serde_json::json!({
        "subagent": "shell-helper",
        "prompt": format!("{CHILD_MARKER}: list the files"),
    })
    .to_string();
    let parent_tools: Arc<Mutex<Option<Vec<serde_json::Value>>>> = Arc::new(Mutex::new(None));
    let child_tools: Arc<Mutex<Option<Vec<serde_json::Value>>>> = Arc::new(Mutex::new(None));
    let (parent_clone, child_clone) = (Arc::clone(&parent_tools), Arc::clone(&child_tools));

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = req.body_json().unwrap_or_default();
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            let tools = body["tools"].as_array().cloned();
            let is_tool_result = messages
                .last()
                .and_then(|message| message["content"].as_array())
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"));
            let is_child = messages
                .first()
                .is_some_and(|message| message.to_string().contains(CHILD_MARKER));
            if is_tool_result {
                sse_response(&text_sse("parent finished"))
            } else if is_child {
                *child_clone.lock().unwrap() = tools;
                sse_response(&text_sse("child finished"))
            } else {
                *parent_clone.lock().unwrap() = tools;
                sse_response(&crate::fixtures::tool_use_sse(
                    "toolu_sub_bg",
                    "invoke_subagent",
                    &invoke_input,
                ))
            }
        })
        .mount(&mock_server)
        .await;

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .env("ANTHROPIC_API_KEY", "test-api-key")
        .env("ANTHROPIC_BASE_URL", mock_server.uri())
        .timeout(std::time::Duration::from_mins(1))
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--no-thread",
            "exec",
            "-m",
            MOCK_MODEL,
            "-p",
            "delegate",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("parent finished"));

    let parent = parent_tools
        .lock()
        .unwrap()
        .clone()
        .expect("parent request");
    let parent_names = sorted_names(&parent);
    assert!(parent_names.contains(&"background_output".to_string()));
    assert!(bash_has_background_property(&parent));

    let child = child_tools.lock().unwrap().clone().expect("child request");
    assert_eq!(sorted_names(&child), vec!["bash", "read"]);
    assert!(!bash_has_background_property(&child));
}
