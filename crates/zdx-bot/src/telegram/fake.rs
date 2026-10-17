//! Scripted fake Bot API for client tests. Never talks to Telegram.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::Path as AxumPath;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

pub(crate) const TOKEN: &str = "123456:test-secret";

/// One scripted answer for a method.
#[derive(Clone)]
pub(crate) enum Reply {
    /// `ok: true`: a fresh message for `send*`/`forwardMessage`, else `true`.
    Ok,
    /// `ok: false` with this code, description, and optional `retry_after`.
    Error(u16, &'static str, Option<u64>),
    /// A body that is not a Bot API envelope (the request may have been
    /// processed).
    Garbage,
    /// `ok: true` whose result does not decode (`message_id` is a string).
    Undecodable,
}

/// A request the fake received: method plus raw body (JSON or multipart).
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) body: String,
    pub(crate) at: std::time::Instant,
}

#[derive(Default)]
struct State {
    scripts: HashMap<String, VecDeque<Reply>>,
    seen: Vec<Seen>,
    next_id: i64,
}

pub(crate) struct FakeTelegram {
    pub(crate) base_url: String,
    state: Arc<Mutex<State>>,
    server: tokio::task::JoinHandle<()>,
}

impl FakeTelegram {
    pub(crate) async fn start() -> Self {
        let state = Arc::new(Mutex::new(State {
            next_id: 100,
            ..State::default()
        }));
        let handler_state = Arc::clone(&state);
        let app = Router::new().route(
            &format!("/bot{TOKEN}/{{method}}"),
            post(move |AxumPath(method): AxumPath<String>, body: Bytes| {
                let state = Arc::clone(&handler_state);
                async move { answer(&state, &method, &body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake Telegram");
        let addr = listener.local_addr().expect("fake Telegram address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve fake Telegram");
        });
        Self {
            base_url: format!("http://{addr}"),
            state,
            server,
        }
    }

    /// Queues answers for `method`; once used up, the method answers `Ok`.
    pub(crate) fn script(&self, method: &str, replies: &[Reply]) {
        self.state
            .lock()
            .expect("fake state")
            .scripts
            .entry(method.to_string())
            .or_default()
            .extend(replies.iter().cloned());
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.state.lock().expect("fake state").seen.clone()
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        self.seen().into_iter().map(|seen| seen.method).collect()
    }
}

impl Drop for FakeTelegram {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn answer(state: &Mutex<State>, method: &str, body: &[u8]) -> Response {
    let mut state = state.lock().expect("fake state");
    state.seen.push(Seen {
        method: method.to_string(),
        body: String::from_utf8_lossy(body).to_string(),
        at: std::time::Instant::now(),
    });
    let reply = state
        .scripts
        .get_mut(method)
        .and_then(VecDeque::pop_front)
        .unwrap_or_else(|| {
            if quotes_an_internal_message_id(method, body) {
                Reply::Error(
                    400,
                    "Bad Request: field \"message_id\" must be a valid Number",
                    None,
                )
            } else {
                Reply::Ok
            }
        });
    match reply {
        Reply::Ok if !(method.starts_with("send") || method == "forwardMessage") => {
            axum::Json(json!({ "ok": true, "result": true })).into_response()
        }
        Reply::Ok => {
            state.next_id += 1;
            let id = state.next_id;
            axum::Json(json!({
                "ok": true,
                "result": {
                    "message_id": id,
                    "chat": { "id": -1001, "type": "supergroup" },
                },
            }))
            .into_response()
        }
        Reply::Error(code, description, retry_after) => {
            let mut body = json!({
                "ok": false,
                "error_code": code,
                "description": description,
            });
            if let Some(secs) = retry_after {
                body["parameters"] = json!({ "retry_after": secs });
            }
            axum::Json::<Value>(body).into_response()
        }
        Reply::Garbage => "<html>502 Bad Gateway</html>".into_response(),
        Reply::Undecodable => axum::Json(json!({
            "ok": true,
            "result": { "message_id": "secret-marker" },
        }))
        .into_response(),
    }
}

/// Mirrors the production refusal seen for synthetic prompts: a rich message
/// whose `reply_parameters` quote an internal countdown id (near `i64::MAX`,
/// beyond what Telegram accepts as a number). Only that form field is read,
/// never uploaded file bytes.
fn quotes_an_internal_message_id(method: &str, body: &[u8]) -> bool {
    const MAX_SAFE_ID: u64 = (1 << 53) - 1;
    const FIELD: &str = "name=\"reply_parameters\"\r\n\r\n";
    if method != "sendRichMessage" {
        return false;
    }
    let body = String::from_utf8_lossy(body);
    let Some(start) = body.find(FIELD).map(|at| at + FIELD.len()) else {
        return false;
    };
    let end = body[start..]
        .find("\r\n")
        .map_or(body.len(), |at| start + at);
    serde_json::from_str::<Value>(&body[start..end])
        .ok()
        .and_then(|value| value.get("message_id").and_then(Value::as_u64))
        .is_some_and(|id| id > MAX_SAFE_ID)
}
