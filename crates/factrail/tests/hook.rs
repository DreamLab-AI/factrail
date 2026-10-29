//! The hook protocol end to end: requests piped into the built binary.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("factrail-hook-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn hook(&self, event: &str, request: &Value) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_factrail"))
            .args(["hook", event])
            .env("HOME", &self.0)
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env_remove("TYPESAFE_API_KEY")
            .env_remove("FACTRAIL_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "the binary exits 0 whenever it answered"
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A session of `n` curl calls whose outputs bury an error line in filler.
fn session(n: usize, email: bool) -> Vec<Value> {
    let mut ms =
        vec![json!({ "role": "user", "text": "find the outage", "toolUses": [], "handle": "h0" })];
    for i in 0..n {
        let tool = if email && i == 1 {
            "mcp__email-gateway__ask_email"
        } else {
            "Bash"
        };
        let body: String = (0..300)
            .map(|k| format!("plain filler line {k}\n"))
            .collect();
        ms.push(json!({ "role": "assistant", "text": "", "handle": format!("a{i}"),
            "toolUses": [{ "tool_use_id": format!("u{i}"), "tool": tool, "input": { "command": format!("curl http://svc/{i}") } }] }));
        ms.push(json!({ "role": "user", "text": "", "toolUses": [], "handle": format!("r{i}"),
            "toolResults": [{ "tool_use_id": format!("u{i}"), "text": format!("{body}HTTP/1.1 503 upstream refused request_id=req-{i:08}\n{body}"), "isError": false }] }));
    }
    ms
}

fn compact_request(config: Value, messages: Vec<Value>, store: Value) -> Value {
    json!({ "protocol": 1, "config": config,
        "session": { "id": "s1", "cwd": "/tmp", "trigger": "auto", "agentId": null },
        "instructions": null, "messages": messages, "usage": { "tokens": 190000, "window": 1000000 }, "store": store })
}

#[test]
fn rules_backend_installs_with_keep_refs_and_saves_outputs() {
    let home = Home::new("rules");
    let r = home.hook(
        "compact",
        &compact_request(json!({ "backend": "rules" }), session(8, false), json!({})),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["action"], "install", "{r}");
    assert_eq!(r["reason"], "rules");
    let list = r["messages"].as_array().unwrap();
    assert_eq!(list.len(), 17);
    assert_eq!(list[0], json!({ "keep": 0 }));
    let reduced = &list[2]["toolResults"][0]["text"];
    let text = reduced.as_str().unwrap();
    assert!(
        text.contains("503 upstream refused request_id=req-00000000"),
        "{text}"
    );
    assert!(text.contains("the full output is saved at"), "{text}");
    assert!(home.0.join("cache/factrail/outputs/s1/u0.txt").exists());
}

#[test]
fn tainted_session_without_a_local_backend_gets_rules_never_the_model() {
    let home = Home::new("taint");
    let config = json!({ "backend": "systemone", "apiKey": "sk-test-not-used-1234567890" });
    let r = home.hook(
        "compact",
        &compact_request(config, session(8, true), json!({})),
    );
    assert_eq!(r["action"], "install", "{r}");
    assert_eq!(r["reason"], "rules:tainted");
    assert_eq!(r["store"]["taint"]["tainted"], true);
    assert!(
        !home.0.join("data/factrail/decisions").exists(),
        "a tainted session is never recorded"
    );
}

#[test]
fn switched_off_and_precompute_and_subagent() {
    let home = Home::new("scope");
    let off = home.hook(
        "compact",
        &compact_request(json!({}), session(4, false), json!({ "enabled": false })),
    );
    assert_eq!(
        (off["action"].as_str(), off["reason"].as_str()),
        (Some("builtin"), Some("switched-off"))
    );
    let mut pre = compact_request(json!({}), session(4, false), json!({}));
    pre["session"]["trigger"] = json!("precompute");
    assert_eq!(home.hook("compact", &pre)["action"], "skip");
    let mut sub = compact_request(json!({}), session(4, false), json!({}));
    sub["session"]["agentId"] = json!("agent-7");
    assert_eq!(home.hook("compact", &sub)["action"], "builtin");
}

#[test]
fn no_key_falls_to_rules_and_summary_fallback_restores_the_old_rule() {
    let home = Home::new("nokey");
    let r = home.hook(
        "compact",
        &compact_request(json!({}), session(8, false), json!({})),
    );
    assert_eq!(
        (r["action"].as_str(), r["reason"].as_str()),
        (Some("install"), Some("rules:no-key"))
    );
    let s = home.hook(
        "compact",
        &compact_request(
            json!({ "fallback": "summary" }),
            session(8, false),
            json!({}),
        ),
    );
    assert_eq!(
        (s["action"].as_str(), s["reason"].as_str()),
        (Some("builtin"), Some("no-key"))
    );
    assert!(
        s["instructions"]
            .as_str()
            .unwrap()
            .contains("saved as files under"),
        "{s}"
    );
}

#[test]
fn protocol_errors_are_answers_not_crashes() {
    let home = Home::new("proto");
    let r = home.hook("compact", &json!({ "protocol": 2 }));
    assert_eq!(r["ok"], false);
    let r = home.hook("nonsense", &json!({ "protocol": 1 }));
    assert_eq!(r["ok"], false);
}

#[test]
fn turn_triggers_holds_and_arms_the_nudge() {
    let home = Home::new("turn");
    let base = |tokens: f64, baseline: Value| {
        json!({ "protocol": 1, "config": {}, "usage": { "tokens": tokens, "percent": 19, "window": 1000000 },
            "baseline": baseline, "rateLimits": 1, "hasApiKey": false, "event": { "reason": "answer" },
            "tools": [{ "tool": "Bash", "skill": null }], "store": {}, "now": 1000 })
    };
    let go = home.hook("turn", &base(190_000.0, Value::Null));
    assert_eq!(go["compact"], true, "{go}");
    let hold = home.hook(
        "turn",
        &base(200_000.0, json!({ "tokens": 190000, "at": 1 })),
    );
    assert_eq!(
        (hold["compact"].as_bool(), hold["reason"].as_str()),
        (Some(false), Some("hysteresis"))
    );
    assert!(hold["nudge"].is_null(), "below the re-arm gap: no nudge");
    let idle = home.hook("turn", &base(150_000.0, Value::Null));
    assert_eq!(idle["compact"], false);
    assert_eq!(idle["nudge"]["delayMs"], 3_300_000, "{idle}");
    let pending = home.hook(
        "turn",
        &base(120_000.0, json!({ "pending": true, "at": 1 })),
    );
    assert_eq!(pending["baseline"]["tokens"], 120_000.0);
    let mut sub = base(190_000.0, Value::Null);
    sub["event"]["agentId"] = json!("a1");
    assert_eq!(home.hook("turn", &sub)["compact"], false);
}

#[test]
fn taint_event_and_status() {
    let home = Home::new("status");
    let t = home.hook("taint", &json!({ "protocol": 1, "config": {}, "tool": null, "skill": "email-search", "store": {}, "now": 5 }));
    assert_eq!(t["taint"]["tainted"], true);
    let again = home.hook("taint", &json!({ "protocol": 1, "config": {}, "tool": "mcp__email-gateway__x", "skill": null, "store": { "taint": t["taint"] }, "now": 9 }));
    assert!(again["taint"].is_null(), "the first record is kept");
    let s = home.hook("status", &json!({ "protocol": 1, "config": {}, "args": "off", "store": {}, "usage": { "tokens": 1000, "window": 200000 }, "tools": [] }));
    assert_eq!(s["store"]["enabled"], false);
    let text = s["text"].as_str().unwrap();
    assert!(text.contains("OFF (saved)"), "{text}");
    assert!(text.contains("built-in summary (switched-off)"), "{text}");
}

/// A fake System One proxy on a loopback port: `POST` answers every question
/// with `noul` 0.9 and names its model and backend; `GET /health` names the
/// backend. Serves until the process ends.
fn fake_proxy(backend: &'static str, model: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body) = loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break (String::new(), Vec::new());
                }
                buf.extend_from_slice(&chunk[..n]);
                let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..i]).into_owned();
                let want = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if buf.len() >= i + 4 + want {
                    break (head, buf[i + 4..i + 4 + want].to_vec());
                }
            };
            let body = if head.starts_with("GET /health") {
                json!({ "ok": true, "backend": backend, "local": false }).to_string()
            } else {
                let asked: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let answers: serde_json::Map<String, Value> = asked["questions"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| (k.clone(), json!({ "noul": 0.9 })))
                    .collect();
                json!({ "answers": answers, "model": model }).to_string()
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-system-one-backend: {backend}\r\nx-system-one-model: jev-latest\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    url
}

#[test]
fn the_decision_log_records_the_model_and_backend_that_answered() {
    let home = Home::new("announce");
    let url = fake_proxy("typesafe", "jev-1.14.0");
    let config = json!({ "backend": "systemone", "baseUrl": url, "model": "subq-snap-preview", "apiKey": "sk-test-not-used-1234567890" });
    let r = home.hook(
        "compact",
        &compact_request(config, session(8, false), json!({})),
    );
    assert_eq!(r["ok"], true, "{r}");
    let dir = home.0.join("data/factrail/decisions");
    let file = std::fs::read_dir(&dir)
        .expect("a decision was logged")
        .next()
        .unwrap()
        .unwrap()
        .path();
    let line = std::fs::read_to_string(file).unwrap();
    let record: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(
        record["judge"]["model"], "jev-1.14.0",
        "the response's model, not the configured one"
    );
    assert_eq!(record["judge"]["backend"], "typesafe");
    assert_eq!(record["judge"]["endpoint"], url.as_str());
}

fn status_text(home: &Home, config: Value) -> String {
    let s = home.hook("status", &json!({ "protocol": 1, "config": config, "args": "", "store": {}, "usage": { "tokens": 1000, "window": 200000 }, "tools": [] }));
    s["text"].as_str().unwrap().to_owned()
}

#[test]
fn status_names_the_live_backend_the_endpoint_and_the_last_judged_model() {
    let home = Home::new("status-live");
    let url = fake_proxy("subq", "jev-1.14.0");
    let config =
        json!({ "backend": "systemone", "baseUrl": url, "apiKey": "sk-test-not-used-1234567890" });
    let before = status_text(&home, config.clone());
    assert!(before.contains(&format!("endpoint: {url}")), "{before}");
    assert!(before.contains("live backend: subq"), "{before}");
    assert!(
        before.contains("no judged compaction recorded yet"),
        "{before}"
    );
    home.hook(
        "compact",
        &compact_request(config.clone(), session(8, false), json!({})),
    );
    let after = status_text(&home, config);
    assert!(after.contains("last judged by jev-1.14.0"), "{after}");
    assert!(!after.contains("vendor cloud"), "{after}");
}

#[test]
fn status_fails_open_when_the_proxy_is_down_and_never_probes_the_default_cloud() {
    let home = Home::new("status-down");
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/systemone", dead.local_addr().unwrap());
    drop(dead);
    let text = status_text(&home, json!({ "backend": "systemone", "baseUrl": url }));
    assert!(
        text.contains("live backend: unknown (proxy unreachable"),
        "{text}"
    );
    assert!(
        text.contains("factrail 0.1.0"),
        "the rest of the status still prints: {text}"
    );

    let default = status_text(&home, json!({ "backend": "systemone" }));
    assert!(
        default.contains("endpoint: https://api.typesafe.ai/v1/systemone (default)"),
        "{default}"
    );
    assert!(
        !default.contains("live backend"),
        "no baseUrl, no probe: {default}"
    );
    let rules = status_text(&home, json!({ "backend": "rules" }));
    assert!(rules.contains("endpoint: none (rules)"), "{rules}");
}
