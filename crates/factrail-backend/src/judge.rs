//! Judges: what answers a [`JudgeRequest`] with one `noul` probability per
//! question.
//!
//! Two wire formats, chosen by enum (no trait objects):
//!
//! * [`SystemOneJudge`] — TypeSafe System One: one `POST` per request carrying
//!   the state and every question; the response's `answers[name].noul` is the
//!   probability.
//! * [`TevJudge`] — a Tev1-format multiple-choice model behind an
//!   OpenAI-compatible `/chat/completions` endpoint: one call per question, the
//!   probability read off the first token's log-probabilities.
//!
//! Both redact the outgoing state and questions with
//! [`redact_value`] (credential shapes plus the judge's own key) and report how
//! many values were masked in [`Answered::redacted`].

use std::collections::HashMap;
use std::error::Error as _;
use std::sync::Arc;
use std::time::Duration;

use factrail_core::JudgeRequest;
use factrail_core::redact::{redact, redact_value};
use factrail_core::tev;
use futures::future::try_join_all;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;

/// The hosted System One endpoint, used when no base URL is given.
pub const SYSTEM_ONE_URL: &str = "https://api.typesafe.ai/v1/systemone";

/// The System One model asked when none is given.
pub const SYSTEM_ONE_MODEL: &str = "jev-latest";

/// Default number of Tev calls in flight at once, per judge.
pub const TEV_CONCURRENCY: usize = 8;

/// Characters of an error response body kept in [`JudgeError::Status`].
const STATUS_BODY_CHARS: usize = 200;

/// Characters of an unexpected answer quoted in [`JudgeError::Malformed`].
const QUOTE_CHARS: usize = 40;

/// Why a judge gave no usable answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JudgeError {
    /// The request could not be sent or its response not read.
    #[error("transport error: {0}")]
    Transport(String),
    /// The round's deadline passed (milliseconds); every in-flight request was dropped.
    #[error("judge deadline of {0} ms passed")]
    Deadline(u64),
    /// The endpoint answered with a non-2xx status.
    #[error("judge answered HTTP {code}: {body}")]
    Status {
        /// The HTTP status code.
        code: u16,
        /// The first 200 characters of the response body, redacted.
        body: String,
    },
    /// The response was not the expected shape.
    #[error("malformed judge response: {0}")]
    Malformed(String),
    /// The response carried no finite `noul` answer for the named question.
    #[error("judge gave no finite answer for {0}")]
    MissingAnswer(String),
}

impl JudgeError {
    /// A short, stable reason for log lines and metrics: `transport`, `deadline`,
    /// `status`, `malformed` or `missing-answer`.
    ///
    /// ```
    /// use factrail_backend::JudgeError;
    /// assert_eq!(JudgeError::Deadline(800).reason(), "deadline");
    /// assert_eq!(JudgeError::MissingAnswer("call_t1".into()).reason(), "missing-answer");
    /// ```
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Transport(_) => "transport",
            Self::Deadline(_) => "deadline",
            Self::Status { .. } => "status",
            Self::Malformed(_) => "malformed",
            Self::MissingAnswer(_) => "missing-answer",
        }
    }
}

/// A judge's answers to one request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answered {
    /// Question name → probability that its statement holds.
    pub answers: HashMap<String, f64>,
    /// Values the outgoing redaction masked in this request.
    pub redacted: usize,
    /// The endpoint's `usage` object, when it reported one (summed over calls for Tev).
    pub usage: Option<Value>,
    /// The model the endpoint says answered, when it said.
    pub model: Option<String>,
}

/// Which judge answered: recorded with every decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeInfo {
    /// `systemone` or `tev`.
    pub kind: String,
    /// The URL requests are posted to.
    pub endpoint: String,
    /// The model asked for.
    pub model: String,
}

/// Exactly what a judge sends for one request: the state and questions after
/// redaction.
#[derive(Clone, Debug, PartialEq)]
pub struct Outgoing {
    /// The redacted state.
    pub state: Value,
    /// The redacted questions, as the System One `questions` object.
    pub questions: Map<String, Value>,
    /// How many values redaction masked.
    pub redacted: usize,
}

/// A judge, dispatched by wire format.
#[derive(Clone, Debug)]
pub enum Judge {
    /// The TypeSafe System One format (hosted or a sovereign façade).
    SystemOne(SystemOneJudge),
    /// A Tev1-format model on an OpenAI-compatible endpoint.
    Tev(TevJudge),
}

impl Judge {
    /// Asks every question of `request`.
    ///
    /// # Errors
    ///
    /// Any [`JudgeError`] but [`JudgeError::Deadline`], which only [`ask_all`] raises.
    pub async fn ask(&self, request: &JudgeRequest) -> Result<Answered, JudgeError> {
        match self {
            Self::SystemOne(j) => j.ask(request).await,
            Self::Tev(j) => j.ask(request).await,
        }
    }

    /// What this judge is, for logs and decision records.
    pub fn info(&self) -> JudgeInfo {
        match self {
            Self::SystemOne(j) => j.info(),
            Self::Tev(j) => j.info(),
        }
    }

    /// The redacted state and questions this judge would send for `request`.
    pub fn outgoing(&self, request: &JudgeRequest) -> Outgoing {
        match self {
            Self::SystemOne(j) => outgoing(request, j.api_key.as_deref()),
            Self::Tev(j) => outgoing(request, j.api_key.as_deref()),
        }
    }
}

/// Asks every request of a round concurrently, under one deadline.
///
/// System One requests all go at once; Tev calls are bounded by the judge's own
/// concurrency, shared across the round. The futures run inside this one, never
/// spawned, so when the deadline passes they are dropped and their connections
/// closed: a late answer cannot be used. Answers come back in request order.
///
/// # Errors
///
/// [`JudgeError::Deadline`] when the round outlives `deadline`; otherwise the
/// first request's error (the others are then dropped).
pub async fn ask_all(
    judge: &Judge,
    requests: &[JudgeRequest],
    deadline: Duration,
) -> Result<Vec<Answered>, JudgeError> {
    let round = try_join_all(requests.iter().map(|r| judge.ask(r)));
    match tokio::time::timeout(deadline, round).await {
        Ok(result) => result,
        Err(_) => Err(JudgeError::Deadline(
            u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
        )),
    }
}

/// A judge speaking the TypeSafe System One wire format.
///
/// `POST <base_url>` with `{"model", "state", "questions"}`; the response must
/// carry `answers[name].noul`, a finite number, for every question asked.
#[derive(Clone, Debug)]
pub struct SystemOneJudge {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl SystemOneJudge {
    /// A System One judge. `base_url` defaults to [`SYSTEM_ONE_URL`], `model` to
    /// [`SYSTEM_ONE_MODEL`]; with no `api_key` the `authorization` header is left
    /// out (a keyless local façade). `timeout` bounds each HTTP request.
    ///
    /// # Panics
    ///
    /// When the TLS backend cannot be initialised, as `reqwest::Client::new` does.
    pub fn new(
        base_url: Option<String>,
        api_key: Option<String>,
        model: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            client: client(timeout),
            base_url: base_url.unwrap_or_else(|| SYSTEM_ONE_URL.to_owned()),
            api_key: api_key.filter(|k| !k.is_empty()),
            model: model.unwrap_or_else(|| SYSTEM_ONE_MODEL.to_owned()),
        }
    }

    /// What this judge is.
    pub fn info(&self) -> JudgeInfo {
        JudgeInfo {
            kind: "systemone".into(),
            endpoint: self.base_url.clone(),
            model: self.model.clone(),
        }
    }

    /// Asks every question of `request` in one call.
    ///
    /// # Errors
    ///
    /// [`JudgeError::Transport`], [`JudgeError::Status`], [`JudgeError::Malformed`]
    /// or [`JudgeError::MissingAnswer`].
    pub async fn ask(&self, request: &JudgeRequest) -> Result<Answered, JudgeError> {
        let out = outgoing(request, self.api_key.as_deref());
        let body = json!({ "model": self.model, "state": out.state, "questions": Value::Object(out.questions) });
        let mut post = self
            .client
            .post(&self.base_url)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(key) = &self.api_key {
            post = post.bearer_auth(key);
        }
        let value = send(post, self.api_key.as_deref()).await?;
        let answers = system_one_answers(&value, request)?;
        Ok(Answered {
            answers,
            redacted: out.redacted,
            usage: value.get("usage").filter(|u| !u.is_null()).cloned(),
            model: value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

/// `parseJevResponse` + `noulAnswer`: `answers` must be an object and every
/// question asked must have a finite `noul`.
fn system_one_answers(
    value: &Value,
    request: &JudgeRequest,
) -> Result<HashMap<String, f64>, JudgeError> {
    let answers = value
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| JudgeError::Malformed("response has no `answers` object".into()))?;
    request
        .questions
        .iter()
        .map(|(name, _)| {
            answers
                .get(name)
                .and_then(|a| a.get("noul"))
                .and_then(Value::as_f64)
                .filter(|p| p.is_finite())
                .map(|p| (name.clone(), p))
                .ok_or_else(|| JudgeError::MissingAnswer(name.clone()))
        })
        .collect()
}

/// A Tev1-format decision model on an OpenAI-compatible `/chat/completions`
/// endpoint (vLLM, llama.cpp server, Together).
///
/// Tev1 (`togethercomputer/tev1`, open weights at
/// `togethercomputer/Tev1-4B-experimental`) is a Qwen3.5-4B fine-tune that
/// picks one option of a multiple-choice decision task. Each `noul` question
/// becomes one call asking whether its statement is true (option `A`) or not
/// (`B`); the probability is `e^lA / (e^lA + e^lB)` over the first token's
/// log-probabilities, falling back to the chosen letter (1 or 0) when the
/// endpoint returns none.
///
/// Honest limits: this is a different and much smaller model than Jev, it was
/// trained on short states (about 2048 tokens) while a fitted factrail state can
/// run to 25 000, and its answers have not been shown to match Jev's. Treat it
/// as a sovereign, local option, and measure it with the eval crate before
/// trusting it.
#[derive(Clone, Debug)]
pub struct TevJudge {
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
    model: String,
    constrain: bool,
    limit: Arc<Semaphore>,
}

impl TevJudge {
    /// A Tev judge posting to `<base_url>/chat/completions` (`base_url` like
    /// `http://host:8000/v1`; a trailing slash is tolerated). At most
    /// `concurrency` calls run at once (0 counts as 1). With `constrain` the body
    /// asks for `response_format: {"type": "regex", "pattern": "(A|B)"}`.
    /// `timeout` bounds each HTTP request.
    ///
    /// ```
    /// use std::time::Duration;
    /// use factrail_backend::TevJudge;
    /// let tev = TevJudge::new("http://127.0.0.1:8000/v1/".into(), None, "tev1".into(), 8, true, Duration::from_secs(20));
    /// assert_eq!(tev.info().endpoint, "http://127.0.0.1:8000/v1/chat/completions");
    /// ```
    ///
    /// # Panics
    ///
    /// When the TLS backend cannot be initialised, as `reqwest::Client::new` does.
    pub fn new(
        base_url: String,
        api_key: Option<String>,
        model: String,
        concurrency: usize,
        constrain: bool,
        timeout: Duration,
    ) -> Self {
        Self {
            client: client(timeout),
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key: api_key.filter(|k| !k.is_empty()),
            model,
            constrain,
            limit: Arc::new(Semaphore::new(concurrency.max(1))),
        }
    }

    /// What this judge is.
    pub fn info(&self) -> JudgeInfo {
        JudgeInfo {
            kind: "tev".into(),
            endpoint: self.url.clone(),
            model: self.model.clone(),
        }
    }

    /// Asks every question of `request`, one call each, within the judge's
    /// concurrency bound.
    ///
    /// # Errors
    ///
    /// [`JudgeError::Transport`], [`JudgeError::Status`] or [`JudgeError::Malformed`]
    /// from the first call that fails.
    pub async fn ask(&self, request: &JudgeRequest) -> Result<Answered, JudgeError> {
        let out = outgoing(request, self.api_key.as_deref());
        let calls = request.questions.iter().map(|(name, _)| {
            let instructions = out
                .questions
                .get(name)
                .and_then(|q| q.get("instructions"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let decision = tev::decision(&out.state, instructions);
            async move { self.ask_one(&decision).await.map(|r| (name.clone(), r)) }
        });
        let replies = try_join_all(calls).await?;
        let mut usage = Map::new();
        let mut model = None;
        let mut answers = HashMap::with_capacity(replies.len());
        for (name, reply) in replies {
            if let Some(Value::Object(u)) = reply.get("usage") {
                for (k, v) in u {
                    if let Some(n) = v.as_u64() {
                        let sum = usage.get(k).and_then(Value::as_u64).unwrap_or(0) + n;
                        usage.insert(k.clone(), Value::from(sum));
                    }
                }
            }
            if model.is_none() {
                model = reply
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            answers.insert(name, tev_probability(&reply)?);
        }
        Ok(Answered {
            answers,
            redacted: out.redacted,
            usage: (!usage.is_empty()).then_some(Value::Object(usage)),
            model,
        })
    }

    async fn ask_one(&self, decision: &str) -> Result<Value, JudgeError> {
        let _permit = self
            .limit
            .acquire()
            .await
            .map_err(|_| JudgeError::Transport("judge closed".into()))?;
        let body = tev_body(&self.model, decision, self.constrain);
        let mut post = self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(key) = &self.api_key {
            post = post.bearer_auth(key);
        }
        send(post, self.api_key.as_deref()).await
    }
}

/// The chat-completions body of one Tev call; `decision` is the user message
/// [`tev::decision`] rendered.
fn tev_body(model: &str, decision: &str, constrain: bool) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": tev::SYSTEM },
            { "role": "user", "content": decision },
        ],
        "temperature": 0,
        "max_tokens": 8,
        "logprobs": true,
        "top_logprobs": 5,
        "chat_template_kwargs": { "enable_thinking": false },
    });
    if constrain {
        let pattern = format!("({}|{})", tev::TRUE_LABEL, tev::FALSE_LABEL);
        body["response_format"] = json!({ "type": "regex", "pattern": pattern });
    }
    body
}

/// The probability that the statement holds, from one chat-completions reply.
fn tev_probability(reply: &Value) -> Result<f64, JudgeError> {
    let choice = reply
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| JudgeError::Malformed("response has no choices".into()))?;
    if let Some(top) = choice
        .pointer("/logprobs/content/0/top_logprobs")
        .and_then(Value::as_array)
    {
        let logprob = |letter: &str| {
            top.iter()
                .find(|e| e.get("token").and_then(Value::as_str).map(str::trim) == Some(letter))
                .and_then(|e| e.get("logprob"))
                .and_then(Value::as_f64)
                .filter(|l| !l.is_nan())
        };
        let p = match (logprob(tev::TRUE_LABEL), logprob(tev::FALSE_LABEL)) {
            // e^a / (e^a + e^b), written so neither exponent can overflow.
            (Some(a), Some(b)) => Some(1.0 / (1.0 + (b - a).exp())),
            (Some(a), None) => Some(a.exp()),
            (None, Some(b)) => Some(1.0 - b.exp()),
            (None, None) => None,
        };
        if let Some(p) = p.filter(|p| p.is_finite()) {
            return Ok(p.clamp(0.0, 1.0));
        }
    }
    match choice
        .pointer("/message/content")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some(l) if l == tev::TRUE_LABEL => Ok(1.0),
        Some(l) if l == tev::FALSE_LABEL => Ok(0.0),
        Some(other) => Err(JudgeError::Malformed(format!(
            "answer is neither A nor B: {:?}",
            other.chars().take(QUOTE_CHARS).collect::<String>()
        ))),
        None => Err(JudgeError::Malformed(
            "response has neither logprobs nor a message".into(),
        )),
    }
}

/// The redacted state and questions of `request`.
fn outgoing(request: &JudgeRequest, api_key: Option<&str>) -> Outgoing {
    let known: Vec<&str> = api_key.into_iter().collect();
    let (state, in_state) = redact_value(&request.state, &known);
    let (questions, in_questions) = redact_value(&Value::Object(request.questions_json()), &known);
    let Value::Object(questions) = questions else {
        unreachable!("redact_value keeps an object an object")
    };
    Outgoing {
        state,
        questions,
        redacted: in_state + in_questions,
    }
}

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("the TLS backend initialises")
}

/// Sends `post` and returns its JSON body, mapping every failure to a [`JudgeError`].
async fn send(post: reqwest::RequestBuilder, api_key: Option<&str>) -> Result<Value, JudgeError> {
    let response = post.send().await.map_err(transport)?;
    let code = response.status().as_u16();
    let text = response.text().await.map_err(transport)?;
    if !(200..300).contains(&code) {
        let known: Vec<&str> = api_key.into_iter().collect();
        let head: String = text.chars().take(STATUS_BODY_CHARS).collect();
        return Err(JudgeError::Status {
            code,
            body: redact(&head, &known).0,
        });
    }
    serde_json::from_str(&text)
        .map_err(|e| JudgeError::Malformed(format!("response is not JSON ({e})")))
}

/// A reqwest error with its source chain (reqwest's own message is terse).
fn transport(error: reqwest::Error) -> JudgeError {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    JudgeError::Transport(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(top: Value, content: &str) -> Value {
        json!({ "choices": [{ "message": { "content": content }, "logprobs": { "content": [{ "token": content, "top_logprobs": top }] } }] })
    }

    #[test]
    fn logprob_maths() {
        let both = reply(
            json!([{ "token": "A", "logprob": -0.1 }, { "token": " B", "logprob": -2.4 }]),
            "A",
        );
        let p = tev_probability(&both).unwrap();
        let (ea, eb) = ((-0.1f64).exp(), (-2.4f64).exp());
        assert!((p - ea / (ea + eb)).abs() < 1e-12);
        let only_a = reply(
            json!([{ "token": "A", "logprob": -0.5 }, { "token": "C", "logprob": -1.0 }]),
            "A",
        );
        assert!((tev_probability(&only_a).unwrap() - (-0.5f64).exp()).abs() < 1e-12);
        let only_b = reply(json!([{ "token": "B\n", "logprob": -0.2 }]), "B");
        assert!((tev_probability(&only_b).unwrap() - (1.0 - (-0.2f64).exp())).abs() < 1e-12);
        let extreme = reply(
            json!([{ "token": "A", "logprob": -1000.0 }, { "token": "B", "logprob": 0.0 }]),
            "B",
        );
        assert_eq!(tev_probability(&extreme).unwrap(), 0.0);
    }

    #[test]
    fn letter_fallback_and_malformed() {
        let a = json!({ "choices": [{ "message": { "content": " A " } }] });
        assert_eq!(tev_probability(&a).unwrap(), 1.0);
        let b = json!({ "choices": [{ "message": { "content": "B" }, "logprobs": null }] });
        assert_eq!(tev_probability(&b).unwrap(), 0.0);
        let no_letters = reply(json!([{ "token": "C", "logprob": -0.1 }]), "B");
        assert_eq!(tev_probability(&no_letters).unwrap(), 0.0);
        let other = json!({ "choices": [{ "message": { "content": "Yes, because" } }] });
        assert_eq!(tev_probability(&other).unwrap_err().reason(), "malformed");
        assert_eq!(
            tev_probability(&json!({ "choices": [] }))
                .unwrap_err()
                .reason(),
            "malformed"
        );
        assert_eq!(
            tev_probability(&json!({ "error": "x" }))
                .unwrap_err()
                .reason(),
            "malformed"
        );
    }

    #[test]
    fn tev_body_shape() {
        let decision = tev::decision(&json!({ "goal": "g" }), "Tool call t1 should stay");
        let body = tev_body("tev1", &decision, false);
        assert_eq!(body["messages"][0]["content"], tev::SYSTEM);
        assert!(body.get("response_format").is_none());
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "model",
                "messages",
                "temperature",
                "max_tokens",
                "logprobs",
                "top_logprobs",
                "chat_template_kwargs"
            ]
        );
        assert_eq!(
            body["messages"][1]["content"],
            r#"{"state":"{\"goal\":\"g\"}","question":"Tool call t1 should stay Is this statement true?","options":[{"label":"A","key":"true","description":"The statement holds"},{"label":"B","key":"false","description":"The statement does not hold"}]}"#
        );
        let constrained = tev_body("tev1", "{}", true);
        assert_eq!(
            constrained["response_format"],
            json!({ "type": "regex", "pattern": "(A|B)" })
        );
    }

    #[test]
    fn reasons() {
        let all = [
            JudgeError::Transport(String::new()),
            JudgeError::Deadline(1),
            JudgeError::Status {
                code: 500,
                body: String::new(),
            },
            JudgeError::Malformed(String::new()),
            JudgeError::MissingAnswer(String::new()),
        ];
        let reasons: Vec<&str> = all.iter().map(JudgeError::reason).collect();
        assert_eq!(
            reasons,
            [
                "transport",
                "deadline",
                "status",
                "malformed",
                "missing-answer"
            ]
        );
    }
}
