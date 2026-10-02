//! Loading evaluation transcripts.
//!
//! Real sessions come from Claude Code's session files. Every session the taint
//! rule fences (an email tool, the email skill) is excluded before it is parsed
//! into anything kept, and a transcript is named by a hash of its path, so a
//! report never carries a path or a project name.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use factrail_core::Message;
use factrail_core::formats::from_claude_jsonl;

/// A named transcript.
pub type Transcript = (String, Vec<Message>);

/// Which sessions to take.
#[derive(Clone, Debug)]
pub struct CorpusOptions {
    /// Fewest tool calls a session must have to be worth compacting.
    pub min_calls: usize,
    /// Most sessions to take.
    pub max: usize,
    /// Tool-name prefixes that fence a session (it is skipped).
    pub taint_tools: Vec<String>,
    /// Skill names whose load fences a session.
    pub taint_skills: Vec<String>,
    /// Salt for the deterministic sample order.
    pub seed: u64,
}

impl Default for CorpusOptions {
    fn default() -> Self {
        Self {
            min_calls: 15,
            max: 40,
            taint_tools: vec![
                "mcp__email-gateway__".into(),
                "mcp__claude_ai_Gmail__".into(),
            ],
            taint_skills: vec!["email-search".into()],
            seed: 0,
        }
    }
}

/// Hex of the first six bytes of a SHA-256: a stable, path-free name.
pub fn short_hash(data: &[u8]) -> String {
    hex::encode(&Sha256::digest(data)[..6])
}

/// True when the session must not be used: any call to a fenced tool, or a
/// `Skill` load of a fenced skill.
pub fn tainted(messages: &[Message], tools: &[String], skills: &[String]) -> bool {
    messages.iter().flat_map(|m| &m.tool_uses).any(|t| {
        tools
            .iter()
            .any(|p| !p.is_empty() && t.tool.starts_with(p.as_str()))
            || (t.tool == "Skill"
                && t.input
                    .get("skill")
                    .or_else(|| t.input.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|s| {
                        skills
                            .iter()
                            .any(|k| s == k || s.ends_with(&format!(":{k}")))
                    }))
    })
}

fn session_files(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            // A subagent's transcript is its own session; the main ones are what compacts.
            if path.file_name().is_some_and(|n| n == "subagents") {
                continue;
            }
            session_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

/// Sessions under a Claude Code projects directory (`~/.claude/projects`), in a
/// deterministic order salted by `seed`, skipping tainted and short ones, up to `max`.
///
/// # Errors
///
/// The directory cannot be listed. Unreadable files are skipped.
pub fn claude_projects(root: &Path, options: &CorpusOptions) -> io::Result<Vec<Transcript>> {
    let mut files = Vec::new();
    session_files(root, &mut files)?;
    let salt = options.seed.to_le_bytes();
    files.sort_by_cached_key(|p| {
        let mut h = Sha256::new();
        h.update(salt);
        h.update(p.to_string_lossy().as_bytes());
        h.finalize().to_vec()
    });
    let mut out = Vec::new();
    for path in files {
        if out.len() >= options.max {
            break;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let messages = from_claude_jsonl(&text);
        let calls = messages.iter().map(|m| m.tool_uses.len()).sum::<usize>();
        if calls < options.min_calls
            || tainted(&messages, &options.taint_tools, &options.taint_skills)
        {
            continue;
        }
        out.push((
            format!("cc-{}", short_hash(path.to_string_lossy().as_bytes())),
            messages,
        ));
    }
    Ok(out)
}

/// Every `*.json` file of `dir` holding a JSON array of messages (the hook's
/// own shape), named by file stem, in name order.
///
/// # Errors
///
/// The directory cannot be listed, or a file is not a message array.
pub fn json_dir(dir: &Path) -> io::Result<Vec<Transcript>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let messages: Vec<Message> =
                serde_json::from_str(&fs::read_to_string(&p)?).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", p.display()))
                })?;
            Ok((
                p.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                messages,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taint_rule_matches_the_policy() {
        let ms: Vec<Message> = serde_json::from_str(
            r#"[{"role":"assistant","text":"","toolUses":[{"tool_use_id":"a","tool":"Skill","input":{"skill":"plugin:email-search"}}]}]"#,
        )
        .unwrap();
        let o = CorpusOptions::default();
        assert!(tainted(&ms, &o.taint_tools, &o.taint_skills));
        let clean: Vec<Message> = serde_json::from_str(
            r#"[{"role":"assistant","text":"","toolUses":[{"tool_use_id":"a","tool":"mcp__email_x","input":{}}]}]"#,
        )
        .unwrap();
        assert!(!tainted(&clean, &o.taint_tools, &o.taint_skills));
    }

    #[test]
    fn loads_jsonl_tree_skipping_subagents_and_taint() {
        let dir = std::env::temp_dir().join(format!("factrail-corpus-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("p/subagents")).unwrap();
        let call = |id: usize, tool: &str| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"id\":\"m{id}\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"t{id}\",\"name\":\"{tool}\",\"input\":{{}}}}]}}}}\n{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t{id}\",\"content\":\"ok\"}}]}}}}\n"
            )
        };
        let clean: String = (0..3).map(|i| call(i, "Bash")).collect();
        let dirty: String = (0..3)
            .map(|i| call(i, "mcp__email-gateway__ask_email"))
            .collect();
        fs::write(dir.join("p/a.jsonl"), &clean).unwrap();
        fs::write(dir.join("p/b.jsonl"), dirty).unwrap();
        fs::write(dir.join("p/subagents/c.jsonl"), &clean).unwrap();
        let o = CorpusOptions {
            min_calls: 2,
            ..CorpusOptions::default()
        };
        let got = claude_projects(&dir, &o).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].0.starts_with("cc-"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
