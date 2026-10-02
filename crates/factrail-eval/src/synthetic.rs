//! A deterministic synthetic corpus of agent sessions.
//!
//! Real transcripts are private and never enter the repository, so the gate a
//! nightly job runs on any machine needs sessions made from code. Each session
//! mixes what the rails must tell apart — long reproducible reads, observations
//! with facts buried in filler, errors, dense tables, logs that change under you
//! — and the agent later reuses some of the facts, so [`crate::facts`] finds them.
//! Same seed, same corpus, on every platform: a splitmix64 generator, no floats
//! in the generation path.

use serde_json::{Map, Value};

use factrail_core::{Message, Role, ToolResult, ToolUse};

/// splitmix64: small, fast, and identical everywhere.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    /// One of `items`.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
    /// A lowercase hex string of `n` digits.
    pub fn hex(&mut self, n: usize) -> String {
        (0..n)
            .map(|_| char::from_digit(self.below(16) as u32, 16).expect("hex digit"))
            .collect()
    }
}

const WORDS: &[&str] = &[
    "the", "service", "returned", "request", "handler", "module", "cache", "config", "value",
    "worker", "queue", "check", "update", "process", "stream", "buffer", "index", "record",
    "schema", "build", "target", "output",
];
const SERVICES: &[&str] = &[
    "api",
    "ingest",
    "billing",
    "search",
    "auth",
    "render",
    "relay",
    "scheduler",
];
const FILES: &[&str] = &[
    "src/main.rs",
    "src/lib.rs",
    "src/handlers/upload.rs",
    "src/store/cache.rs",
    "client/src/app.tsx",
    "config/settings.toml",
    "scripts/deploy.sh",
    "tests/integration.rs",
];

fn filler(rng: &mut Rng, lines: usize) -> Vec<String> {
    (0..lines)
        .map(|_| {
            let n = 6 + rng.below(8) as usize;
            (0..n)
                .map(|_| *rng.pick(WORDS))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn code(rng: &mut Rng, lines: usize) -> String {
    (0..lines)
        .map(|i| match i % 5 {
            0 => format!(
                "fn {}_{}(input: &str) -> Result<(), Error> {{",
                rng.pick(WORDS),
                rng.pick(WORDS)
            ),
            1 => format!(
                "    let {} = {}.{}(input)?;",
                rng.pick(WORDS),
                rng.pick(WORDS),
                rng.pick(WORDS)
            ),
            2 => format!("    // {}", filler(rng, 1)[0]),
            3 => "    Ok(())".to_owned(),
            _ => "}".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splices `facts` into `body` at random line positions.
fn bury(rng: &mut Rng, mut body: Vec<String>, facts: &[String]) -> String {
    for f in facts {
        let at = rng.below(body.len() as u64 + 1) as usize;
        body.insert(at, f.clone());
    }
    body.join("\n")
}

struct Step {
    tool: &'static str,
    input: Map<String, Value>,
    output: String,
    is_error: bool,
    facts: Vec<String>,
}

fn obj(pairs: &[(&str, String)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Value::String(v.clone())))
        .collect()
}

fn step(rng: &mut Rng) -> Step {
    let svc = *rng.pick(SERVICES);
    match rng.below(7) {
        0 => {
            let file = *rng.pick(FILES);
            let lines = 120 + rng.below(220) as usize;
            Step {
                tool: "Read",
                input: obj(&[("file_path", format!("/repo/{file}"))]),
                output: code(rng, lines),
                is_error: false,
                facts: vec![],
            }
        }
        1 => {
            let id = format!("req-{}", rng.hex(10));
            let pid = 10_000 + rng.below(80_000);
            let facts = vec![
                format!("HTTP/1.1 503 Service Unavailable request_id={id}"),
                format!(
                    "upstream {svc} pid {pid} refused connection on port 8{:03}",
                    rng.below(1000)
                ),
            ];
            let lines = 150 + rng.below(250) as usize;
            let body = filler(rng, lines);
            Step {
                tool: "Bash",
                input: obj(&[("command", format!("curl -sv http://{svc}.internal/health"))]),
                output: bury(rng, body, &facts),
                is_error: false,
                facts,
            }
        }
        2 => {
            let failed = 1 + rng.below(4);
            let commit = rng.hex(9);
            let facts = vec![
                format!("test {svc}::tests::round_trip_{} ... FAILED", rng.below(90)),
                format!(
                    "error[E0308]: mismatched types at src/{svc}/mod.rs:{}:{}",
                    10 + rng.below(400),
                    1 + rng.below(80)
                ),
                format!(
                    "test result: FAILED. {} passed; {failed} failed; built at commit {commit}",
                    80 + rng.below(200)
                ),
            ];
            let lines = 200 + rng.below(200) as usize;
            let body = filler(rng, lines);
            Step {
                tool: "Bash",
                input: obj(&[("command", format!("cargo test -p {svc}"))]),
                output: bury(rng, body, &facts),
                is_error: true,
                facts,
            }
        }
        3 => {
            let rows: Vec<String> = (0..60 + rng.below(140))
                .map(|i| {
                    format!(
                        "{svc}-{}-{i:03}  Running  restarts={}  node-{}  10.0.{}.{}",
                        rng.hex(5),
                        rng.below(9),
                        rng.below(40),
                        rng.below(255),
                        rng.below(255)
                    )
                })
                .collect();
            let first = rows[0]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
            Step {
                tool: "Bash",
                input: obj(&[("command", format!("kubectl get pods -l app={svc} -o wide"))]),
                output: rows.join("\n"),
                is_error: false,
                facts: vec![first],
            }
        }
        4 => {
            let trace = format!("trace_id={}", rng.hex(16));
            let facts = vec![format!(
                "ERROR {svc} worker panicked: index out of bounds {trace}"
            )];
            let lines = 300 + rng.below(300) as usize;
            let body = filler(rng, lines);
            Step {
                tool: "Bash",
                input: obj(&[("command", format!("tail -n 400 /var/log/{svc}/server.log"))]),
                output: bury(rng, body, &facts),
                is_error: false,
                facts,
            }
        }
        5 => {
            let hits: Vec<String> = (0..20 + rng.below(80))
                .map(|i| {
                    format!(
                        "/repo/{}:{}: {}",
                        rng.pick(FILES),
                        1 + i * 7,
                        filler(rng, 1)[0]
                    )
                })
                .collect();
            Step {
                tool: "Grep",
                input: obj(&[("pattern", (*rng.pick(WORDS)).to_owned())]),
                output: hits.join("\n"),
                is_error: false,
                facts: vec![],
            }
        }
        _ => {
            let version = format!("{}.{}.{}", 1 + rng.below(4), rng.below(20), rng.below(30));
            let digest = format!("sha256:{}", rng.hex(24));
            let facts = vec![format!(
                "pushed image registry.local/{svc}:{version} digest {digest}"
            )];
            let lines = 40 + rng.below(80) as usize;
            let body = filler(rng, lines);
            Step {
                tool: "Bash",
                input: obj(&[("command", format!("./scripts/deploy.sh {svc}"))]),
                output: bury(rng, body, &facts),
                is_error: false,
                facts,
            }
        }
    }
}

/// One synthetic session of `calls` tool calls.
pub fn session(seed: u64, calls: usize) -> Vec<Message> {
    let mut rng = Rng::new(seed);
    let svc = *rng.pick(SERVICES);
    let mut ms = vec![Message::text(
        Role::User,
        format!("The {svc} service is failing in staging. Find out why and fix it."),
    )];
    let mut learnt: Vec<String> = Vec::new();
    for n in 0..calls {
        let s = step(&mut rng);
        let mut text = String::new();
        let mut input = s.input;
        // The agent acts on what it learnt: quotes an earlier fact, or passes one to a call.
        if !learnt.is_empty() && rng.below(3) == 0 {
            let fact = learnt[rng.below(learnt.len() as u64) as usize].clone();
            let token = fact
                .split_whitespace()
                .find(|w| w.chars().any(|c| c.is_ascii_digit()) && w.len() >= 6)
                .unwrap_or(&fact)
                .to_owned();
            if rng.below(2) == 0 {
                text = format!("Following up on {token} from earlier.");
            } else {
                input.insert(
                    "description".into(),
                    Value::String(format!("check {token}")),
                );
            }
        }
        let id = format!("toolu_{seed:04x}_{n:03}");
        ms.push(Message {
            role: Role::Assistant,
            text,
            tool_uses: vec![ToolUse {
                tool_use_id: id.clone(),
                tool: s.tool.into(),
                input,
                text: Some(s.output.clone()),
                is_error: s.is_error,
            }],
            tool_results: vec![],
        });
        ms.push(Message {
            role: Role::User,
            text: String::new(),
            tool_uses: vec![],
            tool_results: vec![ToolResult {
                tool_use_id: id,
                text: s.output,
                is_error: s.is_error,
            }],
        });
        learnt.extend(s.facts);
        if n % 9 == 8 {
            ms.push(Message::text(
                Role::User,
                format!("Keep going; prioritise the {svc} failures."),
            ));
        }
    }
    ms.push(Message::text(
        Role::Assistant,
        "Summary of the investigation so far.",
    ));
    ms
}

/// `n` sessions of 30–59 calls, seeded `seed, seed + 1, …`.
pub fn corpus(seed: u64, n: usize) -> Vec<(String, Vec<Message>)> {
    (0..n as u64)
        .map(|k| {
            let s = seed.wrapping_add(k);
            let calls = 30 + Rng::new(s ^ 0x5eed).below(30) as usize;
            (format!("synthetic-{s}"), session(s, calls))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_fact_bearing() {
        let a = corpus(7, 3);
        let b = corpus(7, 3);
        assert_eq!(
            a.iter().map(|x| &x.1).collect::<Vec<_>>(),
            b.iter().map(|x| &x.1).collect::<Vec<_>>()
        );
        for (_, s) in &a {
            assert!(!crate::facts::facts(s).is_empty());
            assert!(factrail_core::transcript_chars(s) > 50_000);
        }
    }
}
