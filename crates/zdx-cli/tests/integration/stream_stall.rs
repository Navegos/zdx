//! Timeout protection for model streams that stall, in the main turn and in
//! an `invoke_subagent` child.
//!
//! A raw TCP mock stands in for the provider because the stall cases need a
//! socket that sends part of a response and then holds the connection open,
//! which `wiremock` cannot do.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use predicates::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::fixtures::{MOCK_MODEL, text_sse, tool_use_sse};

fn can_bind_localhost() -> bool {
    std::net::TcpListener::bind("127.0.0.1:0").is_ok()
}

/// SSE prefix with visible text, as a provider sends it before it stalls.
const PARTIAL_TEXT_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-test\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial answer\"}}\n\n",
);

const PING_SSE: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";

/// How the mock answers one request.
enum Reply {
    /// A complete SSE body, then close.
    Full(String),
    /// Headers plus `prefix`, then silence until the client closes.
    Stall(&'static str),
    /// Headers, then a keepalive ping every 200ms until the client closes.
    PingForever,
}

/// What the mock saw on one connection.
#[derive(Clone)]
struct Seen {
    body: Value,
    /// When the client closed a stalled connection, if it did.
    closed_at: Arc<Mutex<Option<Instant>>>,
}

type Router = Arc<dyn Fn(usize, &Value) -> Reply + Send + Sync>;

/// Starts the mock and returns its base URL and the requests it has seen.
async fn start_mock(router: Router) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_accept = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let router = Arc::clone(&router);
            let seen = Arc::clone(&seen_accept);
            tokio::spawn(async move { serve(socket, router, seen).await });
        }
    });
    (base_url, seen)
}

async fn serve(mut socket: TcpStream, router: Router, seen: Arc<Mutex<Vec<Seen>>>) {
    let Some(body) = read_request(&mut socket).await else {
        return;
    };
    let closed_at = Arc::new(Mutex::new(None));
    let index = {
        let mut seen = seen.lock().unwrap();
        seen.push(Seen {
            body: body.clone(),
            closed_at: Arc::clone(&closed_at),
        });
        seen.len() - 1
    };
    match router(index, &body) {
        Reply::Full(sse) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                sse.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(sse.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
        Reply::Stall(prefix) => {
            let _ = socket.write_all(chunked_head().as_bytes()).await;
            let _ = socket.write_all(chunk(prefix).as_bytes()).await;
            // Never write again; record when the client gives up.
            let mut buf = [0u8; 1024];
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            *closed_at.lock().unwrap() = Some(Instant::now());
        }
        Reply::PingForever => {
            let _ = socket.write_all(chunked_head().as_bytes()).await;
            while socket.write_all(chunk(PING_SSE).as_bytes()).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            *closed_at.lock().unwrap() = Some(Instant::now());
        }
    }
}

fn chunked_head() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
}

fn chunk(data: &str) -> String {
    format!("{:x}\r\n{data}\r\n", data.len())
}

/// Reads one HTTP request and returns its JSON body.
async fn read_request(socket: &mut TcpStream) -> Option<Value> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    serde_json::from_slice(&buf[header_end..header_end + length]).ok()
}

fn exec_cmd(zdx_home: &TempDir, root: &TempDir, base_url: &str) -> assert_cmd::Command {
    let mut cmd = crate::fixtures::zdx_cmd();
    cmd.env("ZDX_HOME", zdx_home.path())
        .env("ANTHROPIC_API_KEY", "test-api-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .timeout(Duration::from_mins(1));
    cmd.args([
        "--root",
        root.path().to_str().unwrap(),
        "--no-thread",
        "exec",
    ]);
    cmd.args(["-m", MOCK_MODEL]);
    cmd
}

/// Waits briefly for the mock to notice that the client closed `seen`.
async fn closed_soon(seen: &Seen) -> bool {
    for _ in 0..100 {
        if seen.closed_at.lock().unwrap().is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn write_config(zdx_home: &TempDir, contents: &str) {
    std::fs::write(zdx_home.path().join("config.toml"), contents).unwrap();
}

/// The 2026-10 incident shape: visible output, then a silent open socket.
/// The turn must fail with a stall error after the no-progress limit instead
/// of hanging, and must not retry because output was already shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_that_stalls_after_output_fails_without_retry() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    write_config(&zdx_home, "stream_idle_timeout_secs = 1\n");
    let (base_url, seen) = start_mock(Arc::new(|_, _| Reply::Stall(PARTIAL_TEXT_SSE))).await;

    let started = Instant::now();
    exec_cmd(&zdx_home, &root, &base_url)
        .args(["-p", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("stalled: no output for 1s"));

    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "a shown answer is not retried"
    );
}

/// Keepalive pings must not hide a silent generation. Before any output the
/// stall is retried transparently, and the retry can succeed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_only_stream_is_retried_and_recovers() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    write_config(&zdx_home, "stream_idle_timeout_secs = 1\n");
    let (base_url, seen) = start_mock(Arc::new(|index, _| {
        if index == 0 {
            Reply::PingForever
        } else {
            Reply::Full(text_sse("recovered after stall"))
        }
    }))
    .await;

    exec_cmd(&zdx_home, &root, &base_url)
        .args(["-p", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("recovered after stall"));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(
        closed_soon(&seen[0]).await,
        "the stalled attempt's connection must be closed"
    );
}

const CHILD_MARKER: &str = "CHILD-TASK-7f3a";

fn is_tool_result_turn(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_array())
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
}

fn is_child_request(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .and_then(|messages| messages.first())
        .is_some_and(|message| message.to_string().contains(CHILD_MARKER))
}

/// The incident end to end: an `invoke_subagent` child whose stream stalls
/// after output fails on the stall limit and exits, closing its connection,
/// and the parent turn gets the failure as a tool result and continues.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_subagent_child_fails_and_parent_continues() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    write_config(&zdx_home, "stream_idle_timeout_secs = 1\n");
    let subagent_dir = root.path().join(".zdx").join("subagents");
    std::fs::create_dir_all(&subagent_dir).unwrap();
    std::fs::write(
        subagent_dir.join("tester.md"),
        "---\ndescription: Test subagent\ntools:\n  - read\n---\nYou are the tester subagent.\n",
    )
    .unwrap();

    let invoke_input = json!({
        "subagent": "tester",
        "prompt": format!("{CHILD_MARKER}: inspect the repo"),
    })
    .to_string();
    let (base_url, seen) = start_mock(Arc::new(move |_, body| {
        if is_tool_result_turn(body) {
            Reply::Full(text_sse("parent finished"))
        } else if is_child_request(body) {
            Reply::Stall(PARTIAL_TEXT_SSE)
        } else {
            Reply::Full(tool_use_sse("toolu_sub1", "invoke_subagent", &invoke_input))
        }
    }))
    .await;

    let started = Instant::now();
    exec_cmd(&zdx_home, &root, &base_url)
        .args(["-p", "delegate the inspection"])
        .assert()
        .success()
        .stdout(predicate::str::contains("parent finished"));
    assert!(started.elapsed() < Duration::from_secs(30));

    let seen = seen.lock().unwrap().clone();
    let child = seen
        .iter()
        .find(|request| is_child_request(&request.body))
        .expect("the child must have sent its request");
    assert!(
        closed_soon(child).await,
        "the failed child's connection must be closed"
    );
    let final_turn = seen
        .iter()
        .find(|request| is_tool_result_turn(&request.body))
        .expect("the parent must continue after the child fails");
    assert!(
        final_turn
            .body
            .to_string()
            .contains("stalled: no output for 1s"),
        "tool result must report the stall: {}",
        final_turn.body
    );
}
