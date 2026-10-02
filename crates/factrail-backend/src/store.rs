//! The stores the binary keeps between hook runs: saved full outputs and
//! remembered judge answers. Plain files, `std::fs`, private modes on unix.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use factrail_core::rails::{full_output_note, is_compacted, saved_output_note};
use factrail_core::redact::redact;
use factrail_core::{CallAnswer, Message};
use serde_json::{Map, Value};

/// Saved outputs older than this are deleted by [`OutputStore::expire_if_due`].
pub const OUTPUT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// [`OutputStore::expire_if_due`] runs at most once per this interval.
pub const EXPIRE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Bytes a saved output keeps; a longer one is cut at a character boundary and says so.
pub const OUTPUT_CAP_BYTES: usize = 8 * 1024 * 1024;

/// Answers an [`AnswerStore`] keeps per session.
pub const ANSWER_CAP: usize = 20_000;

/// Characters a sanitised file-name component keeps.
const NAME_CHARS: usize = 200;

/// Name of the marker whose mtime records the last expiry run.
const EXPIRED_MARKER: &str = ".expired";

/// Where factrail keeps its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// Regenerable state: saved outputs, remembered answers.
    pub cache: PathBuf,
    /// Durable data: the decision log.
    pub data: PathBuf,
}

impl Paths {
    /// `$XDG_CACHE_HOME/factrail` (else `$HOME/.cache/factrail`) and
    /// `$XDG_DATA_HOME/factrail` (else `$HOME/.local/share/factrail`). An empty
    /// or relative `XDG_*` value is ignored, as the XDG base-directory
    /// specification requires. `None` when a directory cannot be resolved
    /// (no usable `XDG_*` and no `HOME`).
    pub fn from_env() -> Option<Self> {
        let xdg = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let cache = xdg("XDG_CACHE_HOME").or_else(|| home.as_ref().map(|h| h.join(".cache")))?;
        let data = xdg("XDG_DATA_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".local").join("share")))?;
        Some(Self {
            cache: cache.join("factrail"),
            data: data.join("factrail"),
        })
    }

    /// `<root>/cache/factrail` and `<root>/data/factrail`: a self-contained tree for tests and replays.
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            cache: root.join("cache").join("factrail"),
            data: root.join("data").join("factrail"),
        }
    }
}

/// A session name or tool-use id made safe as one file-name component: every
/// character outside `[A-Za-z0-9_.-]` becomes `_`, a leading `.` becomes `_`
/// (so neither `..` nor a hidden marker can be named), an empty name becomes
/// `_`, and the result is cut to 200 characters.
///
/// ```
/// use factrail_backend::sanitise;
/// assert_eq!(sanitise("toolu_01AbC"), "toolu_01AbC");
/// assert_eq!(sanitise("../../etc/passwd"), "_._.._etc_passwd");
/// assert_eq!(sanitise(".."), "_.");
/// assert_eq!(sanitise(""), "_");
/// ```
pub fn sanitise(name: &str) -> String {
    let mut out: String = name
        .chars()
        .take(NAME_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.starts_with('.') {
        out.replace_range(..1, "_");
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Creates `path` (and parents) and makes it 0700 on unix.
fn private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Opens `path` for writing, created 0600 on unix (and forced to 0600 if it existed).
fn private_file(path: &Path, append: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true);
    if append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

pub(crate) fn private_append(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    private_dir(dir)?;
    let path = dir.join(name);
    private_file(&path, true)?.write_all(bytes)?;
    Ok(path)
}

/// Saved full outputs: `outputs/<session>/<tool_use_id>.txt` under the cache
/// directory, so a reduced result can point at a file the assistant can `Read`.
///
/// Every text is redacted with [`redact`] before it is written and capped at
/// [`OUTPUT_CAP_BYTES`]. Directories are 0700 and files 0600 on unix; session
/// names and ids pass through [`sanitise`], so nothing is written outside the
/// folder.
#[derive(Clone, Debug)]
pub struct OutputStore {
    dir: PathBuf,
}

impl OutputStore {
    /// The store under `paths.cache/outputs`. Nothing is created until a write.
    pub fn new(paths: &Paths) -> Self {
        Self {
            dir: paths.cache.join("outputs"),
        }
    }

    /// The `outputs` folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where `tool_use_id` of `session` is (or would be) saved.
    pub fn path(&self, session: &str, tool_use_id: &str) -> PathBuf {
        self.dir
            .join(sanitise(session))
            .join(format!("{}.txt", sanitise(tool_use_id)))
    }

    /// Writes `text`, redacted and capped, as `tool_use_id`'s saved output.
    ///
    /// # Errors
    ///
    /// Any I/O error creating the folder or writing the file.
    pub fn save(&self, session: &str, tool_use_id: &str, text: &str) -> io::Result<PathBuf> {
        let path = self.path(session, tool_use_id);
        private_dir(&self.dir)?;
        if let Some(parent) = path.parent() {
            private_dir(parent)?;
        }
        let (mut clean, _) = redact(text, &[]);
        if clean.len() > OUTPUT_CAP_BYTES {
            let total = clean.len();
            let mut cut = OUTPUT_CAP_BYTES;
            while !clean.is_char_boundary(cut) {
                cut -= 1;
            }
            clean.truncate(cut);
            clean.push_str(&format!(
                "\n[factrail saved the first {cut} of {total} bytes of this output]\n"
            ));
        }
        let mut file = private_file(&path, false)?;
        file.write_all(clean.as_bytes())?;
        Ok(path)
    }

    /// Saves the full output behind every [`full_output_note`] in `compacted`
    /// and swaps the note for [`saved_output_note`].
    ///
    /// A note is swapped only when `original` holds that call's unreduced
    /// result; reproducible-read notes are included (a file can change after
    /// the read). The same swap is mirrored onto any `tool_uses[].text` carrying
    /// the note, and `touched[i]` is set for each message changed. A failed
    /// write leaves the note as it was. Returns how many outputs were saved.
    pub fn offload(
        &self,
        session: &str,
        original: &[Message],
        compacted: &mut [Message],
        touched: &mut [bool],
    ) -> usize {
        let originals: HashMap<&str, &str> = original
            .iter()
            .flat_map(|m| &m.tool_results)
            .filter(|r| !is_compacted(&r.text))
            .map(|r| (r.tool_use_id.as_str(), r.text.as_str()))
            .collect();
        let mut notes: HashMap<String, Option<String>> = HashMap::new();
        fn mark(touched: &mut [bool], i: usize) {
            if let Some(t) = touched.get_mut(i) {
                *t = true;
            }
        }
        for (i, message) in compacted.iter_mut().enumerate() {
            for result in &mut message.tool_results {
                let note = full_output_note(&result.tool_use_id);
                if !result.text.contains(&note) {
                    continue;
                }
                let Some(text) = originals.get(result.tool_use_id.as_str()) else {
                    continue;
                };
                let saved = notes.entry(result.tool_use_id.clone()).or_insert_with(|| {
                    self.save(session, &result.tool_use_id, text)
                        .ok()
                        .map(|p| saved_output_note(&p.to_string_lossy()))
                });
                if let Some(saved) = saved {
                    result.text = result.text.replace(&note, saved);
                    mark(touched, i);
                }
            }
        }
        for (i, message) in compacted.iter_mut().enumerate() {
            for tool in &mut message.tool_uses {
                let Some(Some(saved)) = notes.get(&tool.tool_use_id) else {
                    continue;
                };
                let note = full_output_note(&tool.tool_use_id);
                if let Some(text) = tool.text.as_mut().filter(|t| t.contains(&note)) {
                    *text = text.replace(&note, saved);
                    mark(touched, i);
                }
            }
        }
        notes.values().filter(|n| n.is_some()).count()
    }

    /// Deletes saved outputs last modified before `now - max_age`, then any
    /// session folder left empty. Returns how many files were deleted. A missing
    /// `outputs` folder is not an error; a file that cannot be removed is skipped.
    ///
    /// # Errors
    ///
    /// When the `outputs` folder exists but cannot be listed.
    pub fn expire(&self, now: SystemTime, max_age: Duration) -> io::Result<usize> {
        let Some(cutoff) = now.checked_sub(max_age) else {
            return Ok(0);
        };
        let sessions = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut deleted = 0;
        for session in sessions.flatten() {
            if !session.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(files) = fs::read_dir(session.path()) else {
                continue;
            };
            for file in files.flatten() {
                let old = file.file_type().is_ok_and(|t| t.is_file())
                    && file
                        .metadata()
                        .and_then(|m| m.modified())
                        .is_ok_and(|mtime| mtime < cutoff);
                if old && fs::remove_file(file.path()).is_ok() {
                    deleted += 1;
                }
            }
            if fs::read_dir(session.path()).is_ok_and(|mut rest| rest.next().is_none()) {
                // A concurrent save may have refilled it; then removal fails harmlessly.
                let _ = fs::remove_dir(session.path());
            }
        }
        Ok(deleted)
    }

    /// [`OutputStore::expire`], at most once per [`EXPIRE_INTERVAL`]: the mtime of
    /// `outputs/.expired` records the last run (set to `now`). `None` when it was
    /// not yet due.
    ///
    /// # Errors
    ///
    /// When listing the folder or writing the marker fails.
    pub fn expire_if_due(&self, now: SystemTime, max_age: Duration) -> io::Result<Option<usize>> {
        let marker = self.dir.join(EXPIRED_MARKER);
        if let Ok(last) = fs::metadata(&marker).and_then(|m| m.modified()) {
            if now
                .duration_since(last)
                .is_ok_and(|age| age < EXPIRE_INTERVAL)
                || last > now
            {
                return Ok(None);
            }
        }
        let deleted = self.expire(now, max_age)?;
        private_dir(&self.dir)?;
        private_file(&marker, false)?.set_modified(now)?;
        Ok(Some(deleted))
    }
}

/// Remembered judge answers, one file per session:
/// `answers/<session>.json` = `{tool_use_id: {"keepCall", "keepResult"}}`.
///
/// A later compaction of the same session does not ask again about a call it
/// already has an answer for. The file keeps insertion order; past
/// [`ANSWER_CAP`] entries the oldest-inserted are dropped (a re-answered call
/// counts as newly inserted, and one save's new answers are inserted in
/// `tool_use_id` order). Writes are atomic (temporary file, then rename) and
/// 0600 on unix.
#[derive(Clone, Debug)]
pub struct AnswerStore {
    dir: PathBuf,
}

impl AnswerStore {
    /// The store under `paths.cache/answers`. Nothing is created until a save.
    pub fn new(paths: &Paths) -> Self {
        Self {
            dir: paths.cache.join("answers"),
        }
    }

    /// Where `session`'s answers are kept.
    pub fn path(&self, session: &str) -> PathBuf {
        self.dir.join(format!("{}.json", sanitise(session)))
    }

    /// `session`'s remembered answers; empty when the file is missing or
    /// unreadable, and entries that are not a valid answer are skipped.
    pub fn load(&self, session: &str) -> HashMap<String, CallAnswer> {
        self.load_ordered(session)
            .into_iter()
            .filter_map(|(id, v)| serde_json::from_value(v).ok().map(|a| (id, a)))
            .collect()
    }

    fn load_ordered(&self, session: &str) -> Map<String, Value> {
        let Ok(text) = fs::read_to_string(self.path(session)) else {
            return Map::new();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map
                .into_iter()
                .filter(|(_, v)| serde_json::from_value::<CallAnswer>(v.clone()).is_ok())
                .collect(),
            _ => Map::new(),
        }
    }

    /// Merges `answers` into `session`'s file and saves it atomically.
    ///
    /// # Errors
    ///
    /// Any I/O error writing the temporary file or renaming it into place.
    pub fn save(&self, session: &str, answers: &HashMap<String, CallAnswer>) -> io::Result<()> {
        let mut map = self.load_ordered(session);
        let mut ids: Vec<&String> = answers.keys().collect();
        ids.sort();
        for id in ids {
            map.shift_remove(id.as_str());
            map.insert(
                id.clone(),
                serde_json::to_value(answers[id]).map_err(io::Error::other)?,
            );
        }
        while map.len() > ANSWER_CAP {
            let Some(oldest) = map.keys().next().cloned() else {
                break;
            };
            map.shift_remove(&oldest);
        }
        private_dir(&self.dir)?;
        let path = self.path(session);
        let temp = self
            .dir
            .join(format!(".{}.{}.tmp", sanitise(session), std::process::id()));
        let written = (|| {
            let mut file = private_file(&temp, false)?;
            file.write_all(Value::Object(map).to_string().as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, &path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use factrail_core::{Role, ToolResult, ToolUse};

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("factrail-backend-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn under_and_sanitise() {
        let p = Paths::under("/r");
        assert_eq!(p.cache, PathBuf::from("/r/cache/factrail"));
        assert_eq!(p.data, PathBuf::from("/r/data/factrail"));
        assert_eq!(sanitise("a/b\\c d:é"), "a_b_c_d__");
        assert_eq!(sanitise(".expired"), "_expired");
        assert_eq!(sanitise(&"x".repeat(500)).len(), 200);
    }

    #[test]
    fn save_is_private_redacted_capped_and_contained() {
        let root = scratch("save");
        let store = OutputStore::new(&Paths::under(&root));
        let path = store
            .save(
                "../escape",
                "../../id",
                "token ghp_abcdefghijklmnopqrstuvwxyz0123 ok",
            )
            .unwrap();
        assert!(path.starts_with(store.dir()));
        assert_eq!(path, store.dir().join("_._escape").join("_._.._id.txt"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "token [REDACTED:gh-token] ok"
        );
        #[cfg(unix)]
        {
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(store.dir()), 0o700);
        }
        let big = "é".repeat(OUTPUT_CAP_BYTES / 2 + 10);
        let saved = fs::read_to_string(store.save("s", "big", &big).unwrap()).unwrap();
        assert!(saved.len() < OUTPUT_CAP_BYTES + 100);
        assert!(saved.ends_with(&format!("of {} bytes of this output]\n", big.len())));
        fs::remove_dir_all(root).unwrap();
    }

    fn pair(id: &str, original: &str, compacted: &str) -> (Message, Message) {
        let result = |text: &str| Message {
            role: Role::User,
            text: String::new(),
            tool_uses: vec![],
            tool_results: vec![ToolResult {
                tool_use_id: id.into(),
                text: text.into(),
                is_error: false,
            }],
        };
        (result(original), result(compacted))
    }

    #[test]
    fn offload_rewrites_notes_and_mirrors() {
        let root = scratch("offload");
        let store = OutputStore::new(&Paths::under(&root));
        let stub = |id: &str| {
            format!(
                "head\n[factrail omitted 900 chars of this tool result; {}]\ntail",
                full_output_note(id)
            )
        };
        let (o1, c1) = pair("u1", "the full output of u1", &stub("u1"));
        let (o2, c2) = pair("u2", "kept verbatim", "kept verbatim");
        let (o3, c3) = pair("u3", &stub("u3"), &stub("u3")); // reduced earlier: the original is unknown
        let call = Message {
            role: Role::Assistant,
            text: String::new(),
            tool_uses: vec![ToolUse {
                tool_use_id: "u1".into(),
                tool: "Bash".into(),
                input: Map::new(),
                text: Some(stub("u1")),
                is_error: false,
            }],
            tool_results: vec![],
        };
        let original = vec![call.clone(), o1, o2, o3];
        let mut compacted = vec![call, c1, c2, c3];
        let mut touched = vec![false; 4];
        let saved = store.offload("sess", &original, &mut compacted, &mut touched);
        assert_eq!(saved, 1);
        assert_eq!(touched, [true, true, false, false]);
        let path = store.path("sess", "u1");
        let note = saved_output_note(&path.to_string_lossy());
        assert_eq!(
            compacted[1].tool_results[0].text,
            stub("u1").replace(&full_output_note("u1"), &note)
        );
        assert!(is_compacted(&compacted[1].tool_results[0].text));
        assert_eq!(
            compacted[0].tool_uses[0].text.as_deref(),
            Some(compacted[1].tool_results[0].text.as_str())
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "the full output of u1");
        assert_eq!(compacted[3].tool_results[0].text, stub("u3"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offload_failure_leaves_the_note() {
        let root = scratch("offload-fail");
        let paths = Paths::under(&root);
        fs::create_dir_all(&paths.cache).unwrap();
        fs::write(
            paths.cache.join("outputs"),
            "a file where the folder should be",
        )
        .unwrap();
        let store = OutputStore::new(&paths);
        let text = format!("x [factrail omitted 5 chars; {}]", full_output_note("u1"));
        let (o, c) = pair("u1", "full", &text);
        let mut compacted = vec![c];
        let mut touched = vec![false];
        assert_eq!(store.offload("s", &[o], &mut compacted, &mut touched), 0);
        assert_eq!(compacted[0].tool_results[0].text, text);
        assert_eq!(touched, [false]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expire_old_outputs_once_a_day() {
        let root = scratch("expire");
        let store = OutputStore::new(&Paths::under(&root));
        let now = SystemTime::now();
        let old = store.save("a", "old", "o").unwrap();
        let fresh = store.save("b", "fresh", "f").unwrap();
        let day = Duration::from_secs(86_400);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(now - 31 * day)
            .unwrap();
        assert_eq!(store.expire_if_due(now, OUTPUT_MAX_AGE).unwrap(), Some(1));
        assert!(!old.exists() && !old.parent().unwrap().exists());
        assert!(fresh.exists());
        assert!(store.dir().join(".expired").exists());
        File::options()
            .write(true)
            .open(&fresh)
            .unwrap()
            .set_modified(now - 31 * day)
            .unwrap();
        assert_eq!(
            store.expire_if_due(now + day / 2, OUTPUT_MAX_AGE).unwrap(),
            None
        );
        assert!(fresh.exists());
        assert_eq!(
            store.expire_if_due(now + day, OUTPUT_MAX_AGE).unwrap(),
            Some(1)
        );
        assert!(!fresh.exists());
        assert_eq!(
            OutputStore::new(&Paths::under(root.join("none")))
                .expire(now, OUTPUT_MAX_AGE)
                .unwrap(),
            0
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn answers_round_trip_merge_cap_and_corruption() {
        let root = scratch("answers");
        let store = AnswerStore::new(&Paths::under(&root));
        assert!(store.load("s").is_empty());
        let a = |p: f64| CallAnswer {
            keep_call: p,
            keep_result: p / 2.0,
        };
        store
            .save(
                "s",
                &HashMap::from([("u1".to_owned(), a(0.9)), ("u2".to_owned(), a(0.2))]),
            )
            .unwrap();
        store
            .save(
                "s",
                &HashMap::from([("u2".to_owned(), a(0.4)), ("u3".to_owned(), a(0.1))]),
            )
            .unwrap();
        let loaded = store.load("s");
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded["u2"], a(0.4));
        let raw: Value =
            serde_json::from_str(&fs::read_to_string(store.path("s")).unwrap()).unwrap();
        assert_eq!(
            raw["u1"],
            serde_json::json!({ "keepCall": 0.9, "keepResult": 0.45 })
        );
        let order: Vec<&String> = raw.as_object().unwrap().keys().collect();
        assert_eq!(order, ["u1", "u2", "u3"]);
        #[cfg(unix)]
        {
            assert_eq!(mode(&store.path("s")), 0o600);
            assert_eq!(mode(store.path("s").parent().unwrap()), 0o700);
        }
        let many: HashMap<String, CallAnswer> = (0..ANSWER_CAP)
            .map(|i| (format!("v{i:05}"), a(0.5)))
            .collect();
        store.save("s", &many).unwrap();
        let capped = store.load("s");
        assert_eq!(capped.len(), ANSWER_CAP);
        assert!(!capped.contains_key("u1") && !capped.contains_key("u3"));
        fs::write(store.path("t"), "{not json").unwrap();
        assert!(store.load("t").is_empty());
        fs::write(
            store.path("t"),
            r#"{"ok": {"keepCall": 1, "keepResult": 0}, "bad": {"keepCall": "x"}}"#,
        )
        .unwrap();
        assert_eq!(store.load("t").len(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
