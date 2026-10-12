//! Tests for `zdx automations run`: non-interactive prompt layers, the runtime
//! context, and the optional per-automation `root`.

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

fn write_automation(zdx_home: &std::path::Path, name: &str, frontmatter: &str) {
    let dir = zdx_home.join("automations");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{name}.md")),
        format!("---\n{frontmatter}---\nSummarize the workspace.\n"),
    )
    .unwrap();
}

fn read_run_records(zdx_home: &std::path::Path) -> Vec<serde_json::Value> {
    fs::read_to_string(zdx_home.join("automations_runs.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn test_automation_run_uses_declared_root_layers_and_runtime_context() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let caller_root = TempDir::new().unwrap();
    fs::write(caller_root.path().join("AGENTS.md"), "CALLER-ROOT-NOTE").unwrap();
    let declared_root = TempDir::new().unwrap();
    fs::write(declared_root.path().join("AGENTS.md"), "DECLARED-ROOT-NOTE").unwrap();
    write_automation(
        zdx_home.path(),
        "scoped",
        &format!(
            "model: \"{MOCK_MODEL}@low\"\nroot: \"{}\"\n",
            declared_root.path().display()
        ),
    );

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
        .current_dir(caller_root.path())
        .args([
            "--root",
            caller_root.path().to_str().unwrap(),
            "--no-thread",
            "automations",
            "run",
            "scoped",
        ])
        .assert()
        .success();

    let request = captured.lock().unwrap().clone().expect("captured request");
    let system = request["system"].to_string();
    assert!(system.contains("DECLARED-ROOT-NOTE"), "{system}");
    assert!(!system.contains("CALLER-ROOT-NOTE"), "{system}");
    assert!(system.contains("This run is non-interactive"), "{system}");
    assert!(system.contains("This run is a ZDX automation"), "{system}");

    let first_user = request["messages"][0].to_string();
    assert!(
        first_user.contains("<runtime_context>"),
        "automation run must carry the runtime-context snapshot: {first_user}"
    );

    let records = read_run_records(zdx_home.path());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["ok"], true);
    assert_eq!(records[0]["model"], format!("{MOCK_MODEL}@low"));
    assert_eq!(
        records[0]["root"],
        declared_root.path().to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn test_automation_run_with_missing_root_fails_without_running() {
    let zdx_home = TempDir::new().unwrap();
    let caller_root = TempDir::new().unwrap();
    let missing = caller_root.path().join("missing-root");
    write_automation(
        zdx_home.path(),
        "scoped",
        &format!("model: \"{MOCK_MODEL}\"\nroot: \"{}\"\n", missing.display()),
    );

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .env("ANTHROPIC_API_KEY", "test-api-key")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .current_dir(caller_root.path())
        .args([
            "--root",
            caller_root.path().to_str().unwrap(),
            "--no-thread",
            "automations",
            "run",
            "scoped",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "does not exist or is not a directory",
        ));

    let records = read_run_records(zdx_home.path());
    assert_eq!(records.len(), 1, "a failed start is still recorded");
    assert_eq!(records[0]["ok"], false);
    assert!(records[0]["root"].is_null());
}

#[test]
fn test_automations_list_json_reports_resolved_settings_and_errors() {
    let zdx_home = TempDir::new().unwrap();
    let caller_root = TempDir::new().unwrap();
    write_automation(
        zdx_home.path(),
        "explicit",
        &format!("schedule: \"0 8 * * *\"\nmodel: \"{MOCK_MODEL}@low\"\n"),
    );
    write_automation(zdx_home.path(), "broken", "root: \"relative/path\"\n");

    let output = crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .current_dir(caller_root.path())
        .args([
            "--root",
            caller_root.path().to_str().unwrap(),
            "automations",
            "list",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let items = list.as_array().unwrap();

    let broken = items.iter().find(|item| item["name"] == "broken").unwrap();
    assert!(broken["settings"].is_null());
    assert!(
        broken["error"]
            .as_str()
            .unwrap()
            .contains("must be an absolute path")
    );

    let explicit = items
        .iter()
        .find(|item| item["name"] == "explicit")
        .unwrap();
    assert_eq!(explicit["declared_model"], format!("{MOCK_MODEL}@low"));
    let settings = &explicit["settings"];
    assert_eq!(settings["model"]["spec"], format!("{MOCK_MODEL}@low"));
    assert_eq!(settings["model"]["origin"], "automation");
    assert_eq!(settings["model"]["thinking"], "low");
    assert_eq!(settings["root"]["origin"], "caller");
    assert_eq!(
        settings["root"]["path"],
        caller_root.path().to_string_lossy().as_ref()
    );
}

/// A subagent body replaces the default system prompt. The automation must
/// still carry project instructions (once), the Safety/Git rules, both
/// non-interactive layers, and the memory index, while keeping the profile's tools.
#[tokio::test]
async fn test_subagent_automation_keeps_safeguards_and_context() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let notes = zdx_home.path().join("memory").join("Notes");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("MEMORY.md"), "MEMORY-INDEX-SENTINEL").unwrap();

    let root = TempDir::new().unwrap();
    fs::write(root.path().join("AGENTS.md"), "PROJECT-RULE-SENTINEL").unwrap();
    let subagents = root.path().join(".zdx").join("subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(
        subagents.join("helper.md"),
        "---\ndescription: Helper\ntools:\n  - read\n---\nHELPER-BODY. {% if project_context %}{{ project_context }}{% endif %}\n",
    )
    .unwrap();
    write_automation(
        zdx_home.path(),
        "delegated",
        &format!("model: \"{MOCK_MODEL}\"\nsubagent: helper\n"),
    );

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
        .current_dir(root.path())
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--no-thread",
            "automations",
            "run",
            "delegated",
        ])
        .assert()
        .success();

    let request = captured.lock().unwrap().clone().expect("captured request");
    let system = request["system"].to_string();
    assert!(system.contains("HELPER-BODY."), "{system}");
    assert_eq!(
        system.matches("PROJECT-RULE-SENTINEL").count(),
        1,
        "{system}"
    );
    assert_eq!(system.matches("## Safety").count(), 1, "{system}");
    assert_eq!(system.matches("## Git").count(), 1, "{system}");
    assert_eq!(
        system.matches("This run is non-interactive").count(),
        1,
        "{system}"
    );
    assert_eq!(
        system.matches("This run is a ZDX automation").count(),
        1,
        "{system}"
    );

    let first_user = request["messages"][0].to_string();
    assert!(first_user.contains("<runtime_context>"), "{first_user}");
    assert!(first_user.contains("MEMORY-INDEX-SENTINEL"), "{first_user}");
    assert!(
        !first_user.contains("<available_skills>"),
        "subagent jobs keep the profile's skill allowlist: {first_user}"
    );
    assert!(
        !first_user.contains("invoke_subagent"),
        "no delegation catalog for a profile without invoke_subagent: {first_user}"
    );

    let tools = request["tools"].as_array().expect("tools array");
    let names: Vec<String> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_lowercase())
        .collect();
    assert_eq!(names, vec!["read".to_string()]);
}

#[test]
fn test_automations_reject_global_model_flag() {
    let zdx_home = TempDir::new().unwrap();
    write_automation(zdx_home.path(), "any", "");

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .args(["--model", MOCK_MODEL, "automations", "run", "any"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--model is not supported for automations",
        ));
}
