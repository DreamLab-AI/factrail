//! Resolution of the plugin's options object into a typed [`Config`].
//!
//! The plugin forwards its `userConfig` options untouched; this module is the
//! only interpreter of them. Every option has a default, and no input — a
//! missing key, a wrong type, a non-finite number — is an error: a malformed
//! option means its default, because the hook must never fail on configuration.
//!
//! # Coercion rules
//!
//! * **Numbers** are read from a JSON number or from a decimal numeric string
//!   (surrounding whitespace ignored), because a shell-projected configuration
//!   carries every scalar as a string. A non-finite result (`"inf"`, `"NaN"`)
//!   or any other shape means the default.
//! * **Counts** (`usize` fields) additionally require a non-negative number and
//!   are floored.
//! * **Keywords** (`backend`, `egress`, `fallback`, `cacheWarm`) are trimmed and
//!   compared case-insensitively.
//! * **Strings** (`baseUrl`, `model`, `apiKey`) must be non-empty strings.
//! * **`backendLocal`** is the exception: it is a data-boundary control, not a
//!   flag, and is `true` only for JSON `true` or the exact string `"true"`.

use serde_json::{Map, Value};

use crate::coerce::{finite_number, js_string, keyword, non_empty_string};

/// Tool-name prefixes whose presence taints a session by default: every tool of
/// the private email gateway, and the hosted Gmail connector.
pub const DEFAULT_TAINT_TOOLS: &[&str] = &["mcp__email-gateway__", "mcp__claude_ai_Gmail__"];

/// Skill names whose loading or expansion taints a session by default.
pub const DEFAULT_TAINT_SKILLS: &[&str] = &["email-search"];

/// Deadline on a whole compaction round, in milliseconds, when none is configured.
pub const DEFAULT_COMPACTION_TIMEOUT_MS: u64 = 15_000;

/// Which judge answers the keep-or-drop questions when the model may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// System One: the TypeSafe service or the sovereign façade (`"systemone"`, the default).
    #[default]
    SystemOne,
    /// A Tev-format, OpenAI-compatible endpoint (`"tev"`).
    Tev,
    /// The deterministic fact-rail rules, which need no model and send nothing (`"rules"`).
    Rules,
}

impl Backend {
    /// Parse the `backend` option. Absent, blank or unrecognised ⇒ [`Backend::SystemOne`].
    ///
    /// ```
    /// use factrail_policy::Backend;
    /// use serde_json::json;
    /// assert_eq!(Backend::parse(Some(&json!(" Tev "))), Backend::Tev);
    /// assert_eq!(Backend::parse(None), Backend::SystemOne);
    /// ```
    pub fn parse(value: Option<&Value>) -> Self {
        match keyword(value).as_deref() {
            Some("tev") => Self::Tev,
            Some("rules") => Self::Rules,
            _ => Self::SystemOne,
        }
    }

    /// The option spelling: `systemone`, `tev` or `rules`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SystemOne => "systemone",
            Self::Tev => "tev",
            Self::Rules => "rules",
        }
    }
}

/// How much of the transcript a model request may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Egress {
    /// The verbatim content the judge needs (`"full"`, the default).
    #[default]
    Full,
    /// Shapes and sizes only, no content (`"metadata"`).
    Metadata,
}

impl Egress {
    /// Parse the `egress` option.
    ///
    /// Absent or blank ⇒ [`Egress::Full`]; `"full"` ⇒ `Full`; `"metadata"` ⇒
    /// `Metadata`. Any **other** string also ⇒ `Metadata`: this option bounds
    /// what leaves the machine, so a mistyped value fails toward sending less.
    ///
    /// ```
    /// use factrail_policy::Egress;
    /// use serde_json::json;
    /// assert_eq!(Egress::parse(None), Egress::Full);
    /// assert_eq!(Egress::parse(Some(&json!("metdata"))), Egress::Metadata);
    /// ```
    pub fn parse(value: Option<&Value>) -> Self {
        match keyword(value).as_deref() {
            None | Some("full") => Self::Full,
            Some(_) => Self::Metadata,
        }
    }

    /// The option spelling: `full` or `metadata`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Metadata => "metadata",
        }
    }
}

/// What happens when the model may not be used (no credentials, or a tainted
/// session on a backend not declared local).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fallback {
    /// Judge with the deterministic rules, which send nothing anywhere (`"rules"`, the default).
    #[default]
    Rules,
    /// Hand the event to Claude Code's built-in summary, the pre-factrail behaviour (`"summary"`).
    Summary,
}

impl Fallback {
    /// Parse the `fallback` option. `"summary"` ⇒ [`Fallback::Summary`]; anything else ⇒ [`Fallback::Rules`].
    pub fn parse(value: Option<&Value>) -> Self {
        match keyword(value).as_deref() {
            Some("summary") => Self::Summary,
            _ => Self::Rules,
        }
    }

    /// The option spelling: `rules` or `summary`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rules => "rules",
            Self::Summary => "summary",
        }
    }
}

/// The cache-warm action taken when a large session sits idle near its prompt-cache expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheWarm {
    /// Compact while the cache is still warm (the default).
    #[default]
    Compact,
    /// Show a toast suggesting `/compact`, and do nothing else.
    Notify,
    /// Do nothing.
    Off,
}

impl CacheWarm {
    /// Parse the `cacheWarm` option the way the plugin always has: the value is
    /// rendered as JavaScript's `String()` would, trimmed and lower-cased;
    /// `off`/`false`/`0`/`no`/`none` ⇒ [`CacheWarm::Off`], `notify`/`toast`/`nudge` ⇒
    /// [`CacheWarm::Notify`], anything else (absent included) ⇒ [`CacheWarm::Compact`].
    ///
    /// ```
    /// use factrail_policy::CacheWarm;
    /// use serde_json::json;
    /// assert_eq!(CacheWarm::parse(Some(&json!(false))), CacheWarm::Off);
    /// assert_eq!(CacheWarm::parse(Some(&json!("Toast"))), CacheWarm::Notify);
    /// assert_eq!(CacheWarm::parse(None), CacheWarm::Compact);
    /// ```
    pub fn parse(value: Option<&Value>) -> Self {
        let text = match value {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string(v),
        };
        match text.trim().to_lowercase().as_str() {
            "off" | "false" | "0" | "no" | "none" => Self::Off,
            "notify" | "toast" | "nudge" => Self::Notify,
            _ => Self::Compact,
        }
    }

    /// The canonical spelling: `compact`, `notify` or `off`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Notify => "notify",
            Self::Off => "off",
        }
    }
}

/// Normalise a list option: an array (each element rendered as JavaScript's
/// `String()`, trimmed, blanks dropped) or a comma-separated string. Any other
/// shape, or a blank string, yields `fallback`.
///
/// An explicit empty array is honoured as an empty list, exactly as before:
/// the operator who writes `[]` has said "no fence" out loud.
///
/// ```
/// use factrail_policy::list_option;
/// use serde_json::json;
/// assert_eq!(list_option(Some(&json!("a, b,,")), &["z"]), ["a", "b"]);
/// assert_eq!(list_option(None, &["z"]), ["z"]);
/// ```
pub fn list_option(value: Option<&Value>, fallback: &[&str]) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => trimmed(items.iter().map(js_string)),
        Some(Value::String(s)) if !s.trim().is_empty() => trimmed(s.split(',').map(str::to_owned)),
        _ => fallback.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// Trim each entry and drop the blank ones.
fn trimmed(items: impl Iterator<Item = String>) -> Vec<String> {
    items
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The compaction deadline in milliseconds: a positive finite number (or
/// numeric string), rounded up to a whole millisecond; anything else ⇒
/// [`DEFAULT_COMPACTION_TIMEOUT_MS`].
///
/// ```
/// use factrail_policy::compaction_timeout_ms;
/// use serde_json::json;
/// assert_eq!(compaction_timeout_ms(Some(&json!(2500))), 2500);
/// assert_eq!(compaction_timeout_ms(Some(&json!(-5))), 15_000);
/// ```
pub fn compaction_timeout_ms(value: Option<&Value>) -> u64 {
    match finite_number(value) {
        Some(ms) if ms > 0.0 => ms.ceil() as u64,
        _ => DEFAULT_COMPACTION_TIMEOUT_MS,
    }
}

/// The resolved configuration. Field documentation names the option it is
/// read from and its default.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// `enabledByDefault`, kept raw: [`crate::resolve_enabled`] interprets it
    /// (boolean, or a string where `0`/`false`/`off`/`no` mean off) only when the
    /// session store holds no switch of its own. Absent ⇒ `None` ⇒ on.
    pub enabled_by_default: Option<Value>,
    /// `taintTools`: tool-name prefixes that taint a session. Default [`DEFAULT_TAINT_TOOLS`].
    pub taint_tools: Vec<String>,
    /// `taintSkills`: skill names that taint a session. Default [`DEFAULT_TAINT_SKILLS`].
    pub taint_skills: Vec<String>,
    /// `backendLocal`: the operator's declaration that the model endpoint is on
    /// this network. `true` only for JSON `true` or the exact string `"true"`;
    /// never inferred from `base_url`. Default `false`.
    pub backend_local: bool,
    /// `backend`: `systemone` (default), `tev` or `rules`.
    pub backend: Backend,
    /// `baseUrl`: endpoint override; `None` ⇒ the backend's default endpoint.
    pub base_url: Option<String>,
    /// `model`: model name; `None` ⇒ the backend's default model.
    pub model: Option<String>,
    /// `apiKey`: a key given in configuration; `None` ⇒ the binary reads its environment.
    pub api_key: Option<String>,
    /// `egress`: `full` (default) or `metadata`.
    pub egress: Egress,
    /// `fallback`: `rules` (default) or `summary`.
    pub fallback: Fallback,
    /// `keepThreshold`: the keep probability above which a span is kept verbatim. Default `0.5`.
    pub keep_threshold: f64,
    /// `preserveRecentMessages`: trailing messages never judged. Default `6`.
    pub preserve_recent_messages: usize,
    /// `maxStateTokens`: the token budget of the state sent per request. Default `25000`.
    pub max_state_tokens: usize,
    /// `maxRequestTokens`: the token budget of one whole request. Default `30000`.
    pub max_request_tokens: usize,
    /// `truncateHeadChars`: characters kept at the head of a reduced output. Default `200`.
    pub truncate_head_chars: usize,
    /// `compactAtPercent`: trigger as a percentage of the context window. Default `60`; `≤ 0` disables it.
    pub compact_at_percent: f64,
    /// `compactAtTokens`: absolute trigger in tokens. Default `180000`; `≤ 0` disables it.
    pub compact_at_tokens: f64,
    /// `rearmTokens`: growth past the post-compaction size needed to trigger again.
    /// Default `40000`; `≤ 0` ⇒ a quarter of the trigger.
    pub rearm_tokens: f64,
    /// `cacheWarm`: the idle-before-expiry action. Default [`CacheWarm::Compact`].
    pub cache_warm: CacheWarm,
    /// `cacheWarmFloorTokens`: context below which no cache-warm nudge is armed. Default `100000`.
    pub cache_warm_floor_tokens: f64,
    /// `cacheTtlSeconds`: prompt-cache TTL; `≤ 0` ⇒ detected. Default `0`.
    pub cache_ttl_seconds: f64,
    /// `cacheTtlMarginSeconds`: how long before expiry the nudge fires. Default `300`.
    pub cache_ttl_margin_seconds: f64,
    /// `compactionTimeoutMs`: deadline on the whole judging round. Default `15000`.
    pub compaction_timeout_ms: u64,
    /// `saveFullOutputs`: keep each reduced tool output on disk. Default `true`.
    pub save_full_outputs: bool,
    /// `recordDecisions`: append judged requests to the decision log. Default `true`.
    pub record_decisions: bool,
    /// `minReductionRatio`: the smallest acceptable reduction. `None` unless
    /// given, in which case the engine derives its gate from window pressure.
    pub min_reduction_ratio: Option<f64>,
}

impl Default for Config {
    fn default() -> Self {
        Self::from_options(&Map::new())
    }
}

impl Config {
    /// Resolve the plugin's options object. Never fails; see the
    /// [module documentation](crate::config) for the coercion rules.
    ///
    /// ```
    /// use factrail_policy::{Backend, Config};
    /// use serde_json::json;
    ///
    /// let options = json!({ "backend": "tev", "compactAtTokens": "120000", "backendLocal": "yes" });
    /// let config = Config::from_options(options.as_object().unwrap());
    /// assert_eq!(config.backend, Backend::Tev);
    /// assert_eq!(config.compact_at_tokens, 120_000.0);
    /// assert!(!config.backend_local, "only true or \"true\" open the fence");
    /// assert_eq!(config.taint_skills, ["email-search"]);
    /// ```
    pub fn from_options(options: &Map<String, Value>) -> Self {
        let get = |key: &str| options.get(key);
        let number = |key: &str, default: f64| finite_number(get(key)).unwrap_or(default);
        let count = |key: &str, default: usize| match finite_number(get(key)) {
            Some(n) if n >= 0.0 => n.floor() as usize,
            _ => default,
        };
        let flag = |key: &str, default: bool| match get(key) {
            Some(Value::Bool(b)) => *b,
            Some(v @ Value::String(_)) => match keyword(Some(v)).as_deref() {
                Some("1" | "true" | "on" | "yes") => true,
                Some("0" | "false" | "off" | "no") => false,
                _ => default,
            },
            _ => default,
        };
        Self {
            enabled_by_default: get("enabledByDefault").cloned(),
            taint_tools: list_option(get("taintTools"), DEFAULT_TAINT_TOOLS),
            taint_skills: list_option(get("taintSkills"), DEFAULT_TAINT_SKILLS),
            backend_local: matches!(get("backendLocal"), Some(Value::Bool(true)))
                || matches!(get("backendLocal"), Some(Value::String(s)) if s == "true"),
            backend: Backend::parse(get("backend")),
            base_url: non_empty_string(get("baseUrl")),
            model: non_empty_string(get("model")),
            api_key: non_empty_string(get("apiKey")),
            egress: Egress::parse(get("egress")),
            fallback: Fallback::parse(get("fallback")),
            keep_threshold: number("keepThreshold", 0.5),
            preserve_recent_messages: count("preserveRecentMessages", 6),
            max_state_tokens: count("maxStateTokens", 25_000),
            max_request_tokens: count("maxRequestTokens", 30_000),
            truncate_head_chars: count("truncateHeadChars", 200),
            compact_at_percent: number("compactAtPercent", 60.0),
            compact_at_tokens: number("compactAtTokens", 180_000.0),
            rearm_tokens: number("rearmTokens", 40_000.0),
            cache_warm: CacheWarm::parse(get("cacheWarm")),
            cache_warm_floor_tokens: number("cacheWarmFloorTokens", 100_000.0),
            cache_ttl_seconds: number("cacheTtlSeconds", 0.0),
            cache_ttl_margin_seconds: number("cacheTtlMarginSeconds", 300.0),
            compaction_timeout_ms: compaction_timeout_ms(get("compactionTimeoutMs")),
            save_full_outputs: flag("saveFullOutputs", true),
            record_decisions: flag("recordDecisions", true),
            min_reduction_ratio: finite_number(get("minReductionRatio")),
        }
    }
}
