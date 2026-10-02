//! The judges against a tiny local HTTP/1.1 server written on a bare
//! `TcpListener`: what goes on the wire, and how every response is read.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use factrail_backend::{Judge, JudgeError, SystemOneJudge, TevJudge, ask_all};
use factrail_core::tev;
use factrail_core::{JudgeRequest, Question};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One request the server received.
#[derive(Clone, Debug)]
struct Seen {
    head: String,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_owned())
        })
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("request body is JSON")
    }
}

/// What the server answers: status, body, and a delay before answering.
type Reply = (u16, String, Duration);

struct Server {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    peak: Arc<AtomicUsize>,
}

async fn serve(respond: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let peak = Arc::new(AtomicUsize::new(0));
    let live = Arc::new(AtomicUsize::new(0));
    let respond = Arc::new(respond);
    let (seen2, peak2) = (seen.clone(), peak.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (seen, peak, live, respond) =
                (seen2.clone(), peak2.clone(), live.clone(), respond.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let split = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..split]).into_owned();
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    })
                    .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < split + length {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let request = Seen {
                    head,
                    body: String::from_utf8_lossy(&buf[split..split + length]).into_owned(),
                };
                seen.lock().unwrap().push(request.clone());
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let (code, body, delay) = respond(&request);
                tokio::time::sleep(delay).await;
                live.fetch_sub(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Server { url, seen, peak }
}

fn ok(body: Value) -> Reply {
    (200, body.to_string(), Duration::ZERO)
}

fn noul(text: &str) -> Question {
    Question {
        kind: "noul".into(),
        instructions: text.into(),
    }
}

fn request(ids: &[&str], goal: &str) -> JudgeRequest {
    let questions = ids
        .iter()
        .flat_map(|id| {
            [
                (
                    format!("call_{id}"),
                    noul(&format!("Tool call {id} (Bash) should stay in the history")),
                ),
                (
                    format!("result_{id}"),
                    noul(&format!(
                        "The full output of tool call {id} (Bash, 900 chars) should stay"
                    )),
                ),
            ]
        })
        .collect();
    JudgeRequest {
        state: json!({ "context": "ctx", "goal": goal, "history": [{ "role": "user", "text": "hi" }] }),
        questions,
        state_tokens: 30,
    }
}

fn system_one(url: &str, key: Option<&str>) -> SystemOneJudge {
    SystemOneJudge::new(
        Some(format!("{url}/v1/systemone")),
        key.map(str::to_owned),
        None,
        Duration::from_secs(5),
    )
}

const KEY: &str = "ts-secret-key-0123456789";

#[tokio::test]
async fn system_one_success_and_wire_format() {
    let server = serve(|_| {
        ok(json!({
            "answers": { "call_t1": { "noul": 0.9 }, "result_t1": { "noul": 0.25 }, "extra": { "noul": 1 } },
            "usage": { "input_tokens": 40 },
            "model": "jev-2"
        }))
    })
    .await;
    let judge = system_one(&server.url, Some(KEY));
    let answered = judge.ask(&request(&["t1"], "ship it")).await.unwrap();
    assert_eq!(answered.answers.len(), 2);
    assert_eq!(answered.answers["call_t1"], 0.9);
    assert_eq!(answered.answers["result_t1"], 0.25);
    assert_eq!(answered.redacted, 0);
    assert_eq!(answered.usage, Some(json!({ "input_tokens": 40 })));
    assert_eq!(answered.model.as_deref(), Some("jev-2"));

    let seen = server.seen.lock().unwrap()[0].clone();
    assert!(seen.head.starts_with("POST /v1/systemone HTTP/1.1"));
    assert_eq!(
        seen.header("authorization").as_deref(),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(
        seen.header("content-type").as_deref(),
        Some("application/json")
    );
    let body = seen.json();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["model", "state", "questions"]);
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["state"]["goal"], "ship it");
    assert_eq!(body["questions"]["call_t1"]["type"], "noul");
    let names: Vec<&String> = body["questions"].as_object().unwrap().keys().collect();
    assert_eq!(names, ["call_t1", "result_t1"]);
}

#[tokio::test]
async fn system_one_without_a_key_sends_no_authorization() {
    let server = serve(|_| {
        ok(json!({ "answers": { "call_t1": { "noul": 0 }, "result_t1": { "noul": 1 } } }))
    })
    .await;
    let answered = system_one(&server.url, None)
        .ask(&request(&["t1"], "g"))
        .await
        .unwrap();
    assert_eq!(answered.answers["result_t1"], 1.0);
    assert_eq!(answered.model, None);
    assert_eq!(server.seen.lock().unwrap()[0].header("authorization"), None);
}

#[tokio::test]
async fn system_one_status_is_reported_with_a_short_body() {
    let long = "x".repeat(500);
    let server = serve(move |_| (503, long.clone(), Duration::ZERO)).await;
    let err = system_one(&server.url, None)
        .ask(&request(&["t1"], "g"))
        .await
        .unwrap_err();
    match &err {
        JudgeError::Status { code, body } => {
            assert_eq!(*code, 503);
            assert_eq!(body.chars().count(), 200);
        }
        other => panic!("expected a status error, got {other:?}"),
    }
    assert_eq!(err.reason(), "status");
}

#[tokio::test]
async fn system_one_malformed_responses() {
    for body in ["not json at all", r#"{"answers": []}"#, r#"{"result": 1}"#] {
        let body = body.to_owned();
        let server = serve(move |_| (200, body.clone(), Duration::ZERO)).await;
        let err = system_one(&server.url, None)
            .ask(&request(&["t1"], "g"))
            .await
            .unwrap_err();
        assert_eq!(err.reason(), "malformed", "{err}");
    }
}

#[tokio::test]
async fn system_one_missing_or_non_finite_answer() {
    let cases = [
        (
            json!({ "answers": { "call_t1": { "noul": 0.5 } } }),
            "result_t1",
        ),
        (
            json!({ "answers": { "call_t1": { "noul": "0.5" }, "result_t1": { "noul": 0.5 } } }),
            "call_t1",
        ),
        (
            json!({ "answers": { "call_t1": 0.5, "result_t1": { "noul": 0.5 } } }),
            "call_t1",
        ),
        (
            json!({ "answers": { "call_t1": { "noul": 0.5 }, "result_t1": { "noul": null } } }),
            "result_t1",
        ),
    ];
    for (body, missing) in cases {
        let server = serve(move |_| ok(body.clone())).await;
        let err = system_one(&server.url, None)
            .ask(&request(&["t1"], "g"))
            .await
            .unwrap_err();
        assert_eq!(err, JudgeError::MissingAnswer(missing.into()));
    }
}

#[tokio::test]
async fn system_one_redacts_on_the_wire() {
    let server = serve(|_| {
        ok(json!({ "answers": { "call_t1": { "noul": 0.5 }, "result_t1": { "noul": 0.5 } } }))
    })
    .await;
    let goal = format!("deploy with {KEY} and ghp_abcdefghijklmnopqrstuvwxyz0123");
    let judge = Judge::SystemOne(system_one(&server.url, Some(KEY)));
    let req = request(&["t1"], &goal);
    let answered = judge.ask(&req).await.unwrap();
    assert_eq!(answered.redacted, 2);
    let seen = server.seen.lock().unwrap()[0].clone();
    assert!(!seen.body.contains(KEY) && !seen.body.contains("ghp_abcdefghij"));
    assert_eq!(
        seen.json()["state"]["goal"],
        "deploy with [REDACTED:known] and [REDACTED:gh-token]"
    );
    assert_eq!(seen.json()["state"], judge.outgoing(&req).state);
}

fn tev(url: &str, concurrency: usize, constrain: bool) -> TevJudge {
    TevJudge::new(
        format!("{url}/v1/"),
        None,
        "tev1".into(),
        concurrency,
        constrain,
        Duration::from_secs(5),
    )
}

fn tev_reply(top: Value, content: &str) -> Value {
    json!({
        "model": "tev1-4b",
        "choices": [{ "message": { "content": content }, "logprobs": { "content": [{ "token": content, "top_logprobs": top }] } }],
        "usage": { "prompt_tokens": 100, "completion_tokens": 1 }
    })
}

#[tokio::test]
async fn tev_logprob_maths_and_wire_format() {
    let server = serve(|seen| {
        let task: Value = serde_json::from_str(seen.json()["messages"][1]["content"].as_str().unwrap()).unwrap();
        let question = task["question"].as_str().unwrap();
        if question.starts_with("Tool call") {
            ok(tev_reply(json!([{ "token": "A", "logprob": (0.8f64).ln() }, { "token": "B", "logprob": (0.2f64).ln() }]), "A"))
        } else {
            ok(tev_reply(json!([{ "token": " B", "logprob": -0.05 }, { "token": "A", "logprob": -3.0 }]), "B"))
        }
    })
    .await;
    let judge = Judge::Tev(tev(&server.url, 8, true));
    let goal = format!("token ghp_abcdefghijklmnopqrstuvwxyz0123 for {KEY}");
    let req = request(&["t1", "t2"], &goal);
    let answered = judge.ask(&req).await.unwrap();
    assert_eq!(answered.answers.len(), 4);
    assert!((answered.answers["call_t1"] - 0.8).abs() < 1e-9);
    let (ea, eb) = ((-3.0f64).exp(), (-0.05f64).exp());
    assert!((answered.answers["result_t2"] - ea / (ea + eb)).abs() < 1e-12);
    assert_eq!(
        answered.usage,
        Some(json!({ "prompt_tokens": 400, "completion_tokens": 4 }))
    );
    assert_eq!(answered.model.as_deref(), Some("tev1-4b"));
    assert_eq!(answered.redacted, 1); // no key configured: only the token shape

    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4);
    let first = &seen[0];
    assert!(first.head.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert_eq!(first.header("authorization"), None);
    let body = first.json();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "model",
            "messages",
            "temperature",
            "max_tokens",
            "logprobs",
            "top_logprobs",
            "chat_template_kwargs",
            "response_format"
        ]
    );
    assert_eq!(body["model"], "tev1");
    assert_eq!(body["temperature"], 0);
    assert_eq!(body["max_tokens"], 8);
    assert_eq!(body["logprobs"], true);
    assert_eq!(body["top_logprobs"], 5);
    assert_eq!(
        body["chat_template_kwargs"],
        json!({ "enable_thinking": false })
    );
    assert_eq!(
        body["response_format"],
        json!({ "type": "regex", "pattern": "(A|B)" })
    );
    assert_eq!(
        body["messages"][0],
        json!({ "role": "system", "content": tev::SYSTEM })
    );
    let user = body["messages"][1]["content"].as_str().unwrap();
    let state = judge.outgoing(&req).state.to_string();
    let questions: Vec<String> = seen
        .iter()
        .map(|s| {
            serde_json::from_str::<Value>(s.json()["messages"][1]["content"].as_str().unwrap())
                .unwrap()["question"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let expected_question = questions
        .iter()
        .find(|q| user.contains(q.as_str()))
        .unwrap();
    assert_eq!(
        user,
        json!({
            "state": state,
            "question": expected_question,
            "options": [
                { "label": "A", "key": "true", "description": "The statement holds" },
                { "label": "B", "key": "false", "description": "The statement does not hold" }
            ]
        })
        .to_string()
    );
    assert!(
        user.starts_with(
            r#"{"state":"{\"context\":\"ctx\",\"goal\":\"token [REDACTED:gh-token] for"#
        )
    );
    assert!(
        questions
            .iter()
            .all(|q| q.ends_with(" Is this statement true?"))
    );
    assert!(!user.contains("ghp_abcdefghij"));
    // Byte-identical to core's renderer, which the dataset export also uses.
    let sent = judge.outgoing(&req);
    let mut on_wire: Vec<String> = seen
        .iter()
        .map(|s| {
            s.json()["messages"][1]["content"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let mut rendered: Vec<String> = sent
        .questions
        .values()
        .map(|q| tev::decision(&sent.state, q["instructions"].as_str().unwrap()))
        .collect();
    on_wire.sort();
    rendered.sort();
    assert_eq!(on_wire, rendered);
}

#[tokio::test]
async fn tev_unconstrained_letter_fallback_and_key() {
    let server = serve(|_| ok(json!({ "choices": [{ "message": { "content": "B" } }] }))).await;
    let judge = TevJudge::new(
        format!("{}/v1", server.url),
        Some(KEY.into()),
        "tev1".into(),
        1,
        false,
        Duration::from_secs(5),
    );
    let answered = judge.ask(&request(&["t1"], "g")).await.unwrap();
    assert_eq!(answered.answers["call_t1"], 0.0);
    assert_eq!(answered.usage, None);
    let seen = server.seen.lock().unwrap()[0].clone();
    assert!(seen.json().get("response_format").is_none());
    assert_eq!(
        seen.header("authorization").as_deref(),
        Some(format!("Bearer {KEY}").as_str())
    );
}

#[tokio::test]
async fn tev_malformed_answers() {
    for body in [
        json!({ "choices": [{ "message": { "content": "Probably yes" } }] }),
        json!({ "choices": [] }),
        json!({ "id": "x" }),
    ] {
        let server = serve(move |_| ok(body.clone())).await;
        let err = tev(&server.url, 2, true)
            .ask(&request(&["t1"], "g"))
            .await
            .unwrap_err();
        assert_eq!(err.reason(), "malformed", "{err}");
    }
}

#[tokio::test]
async fn tev_concurrency_is_bounded_across_a_round() {
    let server = serve(|_| {
        (
            200,
            json!({ "choices": [{ "message": { "content": "A" } }] }).to_string(),
            Duration::from_millis(40),
        )
    })
    .await;
    let judge = Judge::Tev(tev(&server.url, 2, true));
    let requests = vec![
        request(&["t1", "t2"], "g"),
        request(&["t3", "t4"], "g"),
        request(&["t5"], "g"),
    ];
    let answered = ask_all(&judge, &requests, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(answered.len(), 3);
    assert_eq!(server.seen.lock().unwrap().len(), 10);
    assert_eq!(server.peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn ask_all_keeps_request_order() {
    // The first request is answered last.
    let server = serve(|seen| {
        let slow = seen.body.contains("call_t1");
        let p = if slow { 0.1 } else { 0.7 };
        let body = json!({ "answers": {
            "call_t1": { "noul": p }, "result_t1": { "noul": p },
            "call_t2": { "noul": p }, "result_t2": { "noul": p } } });
        (
            200,
            body.to_string(),
            Duration::from_millis(if slow { 150 } else { 0 }),
        )
    })
    .await;
    let judge = Judge::SystemOne(system_one(&server.url, None));
    let answered = ask_all(
        &judge,
        &[request(&["t1"], "g"), request(&["t2"], "g")],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(answered[0].answers["call_t1"], 0.1);
    assert_eq!(answered[1].answers["call_t2"], 0.7);
    assert_eq!(server.peak.load(Ordering::SeqCst), 2); // System One sends the whole round at once
}

#[tokio::test]
async fn ask_all_deadline_drops_the_round() {
    let server = serve(|_| {
        (
            200,
            json!({ "answers": { "call_t1": { "noul": 1 }, "result_t1": { "noul": 1 } } })
                .to_string(),
            Duration::from_secs(3),
        )
    })
    .await;
    let judge = Judge::SystemOne(system_one(&server.url, None));
    let started = Instant::now();
    let err = ask_all(&judge, &[request(&["t1"], "g")], Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(err, JudgeError::Deadline(100));
    assert_eq!(err.reason(), "deadline");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        ask_all(&judge, &[], Duration::from_millis(100))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn ask_all_returns_the_first_error() {
    let server = serve(|_| (500, "boom".into(), Duration::ZERO)).await;
    let judge = Judge::SystemOne(system_one(&server.url, None));
    let err = ask_all(&judge, &[request(&["t1"], "g")], Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        JudgeError::Status {
            code: 500,
            body: "boom".into()
        }
    );
}

#[tokio::test]
async fn unreachable_endpoint_is_a_transport_error() {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let judge = system_one(&format!("http://127.0.0.1:{port}"), None);
    let err = judge.ask(&request(&["t1"], "g")).await.unwrap_err();
    assert_eq!(err.reason(), "transport", "{err}");
}

#[test]
fn info_names_the_judge() {
    let s1 = Judge::SystemOne(SystemOneJudge::new(
        None,
        None,
        None,
        Duration::from_secs(1),
    ));
    assert_eq!(s1.info().kind, "systemone");
    assert_eq!(s1.info().endpoint, "https://api.typesafe.ai/v1/systemone");
    let t = Judge::Tev(tev("http://h:8000", 4, true));
    assert_eq!(t.info().kind, "tev");
    assert_eq!(t.info().endpoint, "http://h:8000/v1/chat/completions");
    assert_eq!(t.info().model, "tev1");
}
