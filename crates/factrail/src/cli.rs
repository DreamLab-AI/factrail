//! Command-line arguments.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Verbatim, fact-keeping context compaction for coding agents.
#[derive(Debug, Parser)]
#[command(name = "factrail", version, about)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve one Claude Code hook request: JSON on stdin, JSON on stdout (docs/protocol.md).
    Hook {
        /// `compact`, `turn`, `taint` or `status`.
        event: String,
    },
    /// Compact one transcript file and print the result.
    Compact(CompactArgs),
    /// Replay a corpus: compact at cuts and count the facts the agent goes on to use.
    Eval(EvalArgs),
    /// Export Tev1-format training records labelled by hindsight.
    Dataset(DatasetArgs),
    /// Manage saved full outputs.
    Outputs {
        /// What to do with them.
        #[command(subcommand)]
        action: OutputsAction,
    },
}

/// `outputs` actions.
#[derive(Debug, Subcommand)]
pub enum OutputsAction {
    /// Delete saved outputs older than `--days`.
    Expire {
        /// Age in days.
        #[arg(long, default_value_t = 30)]
        days: u64,
    },
}

/// Who judges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum JudgeKind {
    /// No model: the rails alone.
    Rules,
    /// Hindsight labels: an upper bound on any judge (eval only).
    Oracle,
    /// Answers remembered in the decision log (eval only).
    Recorded,
    /// A System One endpoint (TypeSafe, or the sovereign façade).
    Systemone,
    /// A Tev-format model on an OpenAI-compatible endpoint.
    Tev,
}

/// Judge selection.
#[derive(Debug, Args)]
pub struct JudgeArgs {
    /// The judge.
    #[arg(long, value_enum, default_value = "rules")]
    pub judge: JudgeKind,
    /// Endpoint (System One URL, or the OpenAI-compatible `/v1` base for Tev).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Model name.
    #[arg(long)]
    pub model: Option<String>,
    /// Declare the endpoint local (allows a keyless System One façade).
    #[arg(long)]
    pub local: bool,
    /// Deadline per judging round, in seconds.
    #[arg(long, default_value_t = 120)]
    pub deadline_secs: u64,
}

/// Engine options.
#[derive(Debug, Args)]
pub struct EngineArgs {
    /// Keep probability threshold.
    #[arg(long, default_value_t = 0.5)]
    pub keep_threshold: f64,
    /// Newest messages pinned.
    #[arg(long, default_value_t = 6)]
    pub preserve_recent: usize,
    /// Token ceiling for the judge's state.
    #[arg(long, default_value_t = 25_000)]
    pub max_state_tokens: usize,
    /// Token ceiling for one request.
    #[arg(long, default_value_t = 30_000)]
    pub max_request_tokens: usize,
    /// Send only the shape of tool arguments, never their values.
    #[arg(long)]
    pub metadata_egress: bool,
    /// Choose fact lines by regex score instead of learned token value.
    #[arg(long)]
    pub regex_lines: bool,
    /// Do not pool one fact budget across stubs.
    #[arg(long)]
    pub no_pool: bool,
}

/// Where a corpus comes from (one of).
#[derive(Debug, Args)]
pub struct Source {
    /// The deterministic synthetic corpus, this many sessions.
    #[arg(long)]
    pub synthetic: Option<usize>,
    /// A Claude Code projects directory (default `~/.claude/projects`).
    #[arg(long)]
    pub claude_projects: Option<PathBuf>,
    /// A directory of `*.json` message arrays.
    #[arg(long)]
    pub json_dir: Option<PathBuf>,
    /// Most real sessions to take.
    #[arg(long, default_value_t = 40)]
    pub max: usize,
    /// Fewest tool calls a real session must have.
    #[arg(long, default_value_t = 15)]
    pub min_calls: usize,
    /// Seed (synthetic corpus; sample order for real sessions).
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
}

/// `compact` arguments.
#[derive(Debug, Args)]
pub struct CompactArgs {
    /// Transcript: `.jsonl` (Claude Code), or JSON (hook messages or OpenAI chat).
    pub input: PathBuf,
    /// Write the result here instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Target reduction for the rails.
    #[arg(long, default_value_t = 0.3)]
    pub target: f64,
    /// Hard line: evict the oldest results past this reduction (0 never evicts).
    #[arg(long, default_value_t = 0.3)]
    pub hard: f64,
    /// Save reduced outputs under this session name.
    #[arg(long)]
    pub save_outputs: bool,
    /// Session name for saved outputs.
    #[arg(long, default_value = "cli")]
    pub session: String,
    /// Judge.
    #[command(flatten)]
    pub judge: JudgeArgs,
    /// Engine.
    #[command(flatten)]
    pub engine: EngineArgs,
}

/// `eval` arguments.
#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Corpus.
    #[command(flatten)]
    pub source: Source,
    /// Judge.
    #[command(flatten)]
    pub judge: JudgeArgs,
    /// Engine.
    #[command(flatten)]
    pub engine: EngineArgs,
    /// Cuts, as fractions of each transcript.
    #[arg(long, value_delimiter = ',', default_value = "0.5,0.75")]
    pub cuts: Vec<f64>,
    /// Target reduction.
    #[arg(long, default_value_t = 0.3)]
    pub target: f64,
    /// Hard line.
    #[arg(long, default_value_t = 0.3)]
    pub hard: f64,
    /// Also simulate repeated compactions at 0.8–1.5 windows.
    #[arg(long)]
    pub sim: bool,
    /// Write the full report as JSON.
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// Check the totals against this baseline; exit 1 on failure.
    #[arg(long)]
    pub gate: Option<PathBuf>,
    /// Write the totals as a new baseline.
    #[arg(long)]
    pub write_baseline: Option<PathBuf>,
    /// Tolerance recorded in a written baseline.
    #[arg(long, default_value_t = 0.005)]
    pub tolerance: f64,
}

/// `dataset` arguments.
#[derive(Debug, Args)]
pub struct DatasetArgs {
    /// Corpus.
    #[command(flatten)]
    pub source: Source,
    /// Output directory (must not exist).
    #[arg(long)]
    pub out: PathBuf,
    /// Share of transcripts in dev.
    #[arg(long, default_value_t = 0.1)]
    pub dev_share: f64,
    /// State budget per decision: keep it inside the model's sequence limit.
    #[arg(long, default_value_t = 1_500)]
    pub max_state_tokens: usize,
    /// Add teacher labels from the decision log (check the judge vendor's terms first).
    #[arg(long)]
    pub teacher: bool,
}
