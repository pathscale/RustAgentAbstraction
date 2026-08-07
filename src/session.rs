//! Binding a caller-owned session *name* to an agent's native session id.
//!
//! A consumer threads one stable name ("thread-42") across turns; this module
//! keeps the mapping to whatever handle the agent actually understands, so the
//! consumer never extracts or re-passes an id itself.
//!
//! Two agents let the caller **mint** the id ([`SessionSupport::Minted`]):
//! Claude via `--session-id`, Copilot via the same flag in both directions. For
//! those the binding is written *before* the process starts, so a run that
//! crashes mid-turn still leaves a resumable session. Codex only **prints** its
//! `thread_id`, so its binding can only be recorded after the run produced one.
//!
//! Layout is one JSON file per session, `<dir>/<project-slug>/<name>.json`,
//! partitioned by project so the same name in two checkouts never collides.
//! Writes go through a temp file and a rename, so a concurrent reader never sees
//! a half-written record.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::{Agent, Continue, SessionSupport};
use crate::error::{Error, Result};

/// One named conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionRecord {
    /// The caller's stable name, exactly as they supplied it. The on-disk
    /// filename is an encoded form of this; the record keeps the original so a
    /// listing hands back the name the caller actually used.
    pub name: String,
    /// The project this session belongs to.
    pub project: String,
    /// The agent that owns it. A session cannot migrate between agents.
    pub agent: Agent,
    /// The agent's native handle, which the next turn resumes with.
    pub token: String,
    /// Unix epoch seconds when the session was first created.
    pub created: i64,
    /// Unix epoch seconds of the most recent turn.
    pub updated: i64,
}

/// Whether the next turn starts a conversation or continues one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No prior record: this turn opens the conversation.
    Create,
    /// A record exists: this turn appends to it.
    Continue,
    /// A record exists and this turn branches off it, leaving it untouched.
    Fork,
}

/// The store of named sessions.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

/// An exclusive lease on one named session.
///
/// The file remains on disk after release, but the OS lock does not: closing
/// the handle, including when a process is killed, releases it. Keeping the
/// inert file avoids an unlink race where a new opener could lock a different
/// inode while the prior holder still owns the old one.
pub(crate) struct SessionLease {
    file: fs::File,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

impl SessionStore {
    /// A store rooted at `dir`. The directory is created lazily on first write.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The default per-user location: `$XDG_STATE_HOME/agent-abstraction/sessions`
    /// (falling back to `~/.local/state`), or `%LOCALAPPDATA%` on Windows.
    /// `None` when neither the platform state dir nor `$HOME` can be resolved.
    #[must_use]
    pub fn default_dir() -> Option<PathBuf> {
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
                })
        };
        Some(base?.join("agent-abstraction").join("sessions"))
    }

    /// The file backing `name` for `project`. Pure path arithmetic.
    ///
    /// The agent is **not** part of the key on purpose: one name must resolve to
    /// one file across agents, so asking for a Claude session under a name
    /// Codex already owns is a loud [`Error::SessionConflict`] rather than two
    /// unrelated conversations quietly sharing a name.
    #[must_use]
    pub fn path_of(&self, project: &Path, name: &str) -> PathBuf {
        self.dir
            .join(project_slug(project))
            .join(format!("{}.json", encode_segment(name)))
    }

    /// Claim one named session until the returned guard is dropped.
    ///
    /// Advisory file locks are cross-process and are released by the OS when a
    /// holder dies, so a crashed agent host cannot strand a stale lease. The
    /// lock is non-blocking: a queue or server can report contention and decide
    /// its own retry policy instead of tying up a worker indefinitely.
    pub(crate) fn lease(&self, project: &Path, name: &str) -> Result<SessionLease> {
        let path = self.path_of(project, name).with_extension("lock");
        let store_err = |source| Error::Store {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(store_err)?;
            restrict_to_owner(parent).map_err(store_err)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(store_err)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(SessionLease { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(Error::SessionBusy {
                    name: name.to_string(),
                    project: project.display().to_string(),
                })
            }
            Err(error) => Err(store_err(error)),
        }
    }

    /// The stored record, or `None` when there is none.
    ///
    /// Only a genuinely absent file is `Ok(None)`. A permission error, an I/O
    /// failure or a corrupt record is an [`Error::Store`], because treating
    /// those as "no session" silently starts a new conversation and abandons
    /// one the caller believes they are still in.
    ///
    /// # Errors
    /// [`Error::Store`] if the record exists but cannot be read or parsed.
    pub fn get(&self, project: &Path, name: &str) -> Result<Option<SessionRecord>> {
        let path = self.path_of(project, name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::Store {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| Error::Store {
                path: path.display().to_string(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            })
    }

    /// Every session recorded for `project`, in unspecified order.
    ///
    /// A record that cannot be read or parsed is an error rather than an
    /// omission: silently returning a short list makes a corrupt store look
    /// like a store with fewer sessions. Use [`SessionStore::list_lossy`] when
    /// skipping bad records is genuinely what you want.
    ///
    /// # Errors
    /// [`Error::Store`] if the directory or any record within it is unreadable.
    pub fn list(&self, project: &Path) -> Result<Vec<SessionRecord>> {
        let dir = self.dir.join(project_slug(project));
        let store_err = |path: &Path, source| Error::Store {
            path: path.display().to_string(),
            source,
        };
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // No directory means no sessions, which is not a fault.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(store_err(&dir, e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| store_err(&dir, e))?.path();
            // Only records belong in the result. Temp files can be present
            // during an atomic write and `.lock` files persist so every process
            // coordinates on the same inode.
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let text = fs::read_to_string(&path).map_err(|e| store_err(&path, e))?;
            out.push(serde_json::from_str(&text).map_err(|e| {
                store_err(
                    &path,
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?);
        }
        Ok(out)
    }

    /// Every readable session for `project`, skipping any that are not.
    ///
    /// The deliberately lossy counterpart to [`SessionStore::list`], for a UI
    /// that would rather show the sessions it can than fail the whole listing.
    #[must_use]
    pub fn list_lossy(&self, project: &Path) -> Vec<SessionRecord> {
        let dir = self.dir.join(project_slug(project));
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|text| serde_json::from_str(&text).ok())
            .collect()
    }

    /// Decide how `name` continues, and produce the continuation to run with.
    ///
    /// For a minting agent with no prior record this allocates the id here and
    /// now, so the caller can persist the binding before spawning.
    ///
    /// Crate-internal: it returns `Continue`, which is machinery rather than
    /// API. Callers reach this through [`crate::Request::session`], and can see
    /// the decision it made via [`crate::Request::session_phase`].
    ///
    /// # Errors
    /// [`Error::SessionConflict`] when the name already belongs to another
    /// agent; [`Error::Unsupported`] when `fork` is asked of an agent that
    /// cannot fork, or when the agent exposes no session id at all.
    pub(crate) fn plan(
        &self,
        agent: Agent,
        project: &Path,
        name: &str,
        fork: bool,
    ) -> Result<(Phase, Continue)> {
        let caps = agent.caps();
        if caps.session == SessionSupport::None {
            return Err(Error::Unsupported {
                agent,
                what: "named sessions (it exposes no session id headlessly)",
            });
        }
        let existing = self.get(project, name)?;
        if let Some(record) = &existing {
            if record.agent != agent {
                return Err(Error::SessionConflict {
                    name: name.to_string(),
                    bound: record.agent,
                    requested: agent,
                });
            }
        }

        Ok(match (existing, fork) {
            (Some(record), true) => {
                if !caps.fork {
                    return Err(Error::Unsupported {
                        agent,
                        what: "forking a session headlessly",
                    });
                }
                (Phase::Fork, Continue::Fork(record.token))
            }
            (Some(record), false) => (Phase::Continue, Continue::Resume(record.token)),
            // Forking a conversation that does not exist yet is just starting
            // one; there is nothing to branch from.
            (None, _) => (
                Phase::Create,
                match caps.session {
                    SessionSupport::Minted => Continue::NewWith(Uuid::new_v4().to_string()),
                    // The id only exists once the agent prints it.
                    SessionSupport::Printed | SessionSupport::None => Continue::New,
                },
            ),
        })
    }

    /// Record `token` as the handle for `name`, preserving the original
    /// creation time when the session already existed.
    ///
    /// # Errors
    /// [`Error::Store`] if the record cannot be written.
    pub fn bind(
        &self,
        agent: Agent,
        project: &Path,
        name: &str,
        token: &str,
    ) -> Result<SessionRecord> {
        // The same invariant `plan` enforces, applied here too: `bind` is
        // public, so the check cannot live only on the path that happens to
        // call it first.
        if let Some(existing) = self.get(project, name)?
            && existing.agent != agent
        {
            return Err(Error::SessionConflict {
                name: name.to_string(),
                bound: existing.agent,
                requested: agent,
            });
        }

        let now = now_secs();
        let record = SessionRecord {
            name: name.to_string(),
            project: project.display().to_string(),
            agent,
            token: token.to_string(),
            created: self.get(project, name)?.map_or(now, |r| r.created),
            updated: now,
        };

        let path = self.path_of(project, name);
        let store_err = |source| Error::Store {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(store_err)?;
            restrict_to_owner(parent).map_err(store_err)?;
        }
        let mut text = serde_json::to_string_pretty(&record)
            .map_err(|e| store_err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        text.push('\n');

        // Write beside the target and rename, so a reader never observes a
        // partial record. The temp name carries the pid and a counter: a single
        // shared `<name>.json.tmp` would let two concurrent writers for the same
        // session scribble over each other's half-written file and then rename
        // the result into place.
        let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), next_temp_id()));
        write_private(&tmp, text.as_bytes()).map_err(store_err)?;
        // Rename is atomic within a directory, so the last writer wins cleanly
        // rather than producing a torn record.
        fs::rename(&tmp, &path).map_err(|e| {
            // Do not leave the temp file behind if the rename failed.
            let _ = fs::remove_file(&tmp);
            store_err(e)
        })?;
        // Syncing the file persists its contents, not the directory entry that
        // names it. Without this a crash can leave the rename unrecorded and
        // the session lost, which is the failure this store exists to avoid.
        if let Some(parent) = path.parent() {
            sync_dir(parent).map_err(store_err)?;
        }
        Ok(record)
    }

    /// Drop the binding for `name`. Removing an absent session is not an error.
    ///
    /// # Errors
    /// [`Error::Store`] if an existing record cannot be removed.
    pub fn forget(&self, project: &Path, name: &str) -> Result<()> {
        let path = self.path_of(project, name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Store {
                path: path.display().to_string(),
                source,
            }),
        }
    }
}

/// A per-process counter making each temp filename unique, so concurrent writes
/// to one session cannot share a scratch file.
fn next_temp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Write `bytes` to a newly created file that only the owner can read.
///
/// Session tokens resume conversations, so they are closer to a credential than
/// to a cache entry and should not be readable by other users on the machine.
/// Permissions are set at creation rather than afterwards, leaving no window
/// where the file exists world-readable.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    // Flush to disk before the rename, so a crash cannot leave an empty record
    // where a valid one is expected.
    file.sync_all()
}

/// Flush a directory entry to disk. A no-op where directories cannot be opened
/// for syncing, which is the case on Windows.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Restrict a directory to its owner. A no-op on platforms without Unix modes,
/// where the parent directory's inherited ACL governs instead.
fn restrict_to_owner(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Seconds since the epoch. A pre-1970 clock reads as 0 rather than panicking;
/// these timestamps are for display, not for correctness.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The longest encoded stem written before truncation applies. Filenames cap
/// near 255 bytes on every filesystem this targets, leaving room for the
/// disambiguating suffix and the extension.
const MAX_STEM: usize = 200;

/// Encode an arbitrary name as one filesystem-safe path segment, injectively.
///
/// Percent-encodes every byte outside `[a-z0-9._-]`, which keeps the mapping
/// reversible and, more importantly, **collision-free**. A lossy scheme that
/// folded unsafe characters to `-` would map `café` and `cafe-` onto one file,
/// and the second session to use that name would silently resume the first
/// one's conversation.
///
/// Uppercase letters are encoded rather than lowercased because macOS and
/// Windows are case-insensitive: leaving them intact would let `Chat` and `chat`
/// collide on exactly the platforms this crate targets. `%` is itself always
/// encoded, so an escape marker is unambiguous and no literal character can be
/// mistaken for one.
///
/// The tradeoff this makes, deliberately: an ASCII name stays readable on disk
/// (`greet-flow.json`), while a non-ASCII one becomes verbose, since every byte
/// outside the safe set costs three characters and a multi-byte character
/// several of those (`日本語` encodes to 27). Readability is a debugging
/// convenience; a collision resumes the wrong conversation. So the scheme keeps
/// names distinguishable first and legible second, and a caller who wants
/// pretty filenames should choose ASCII names.
///
/// Names too long to encode whole are truncated and disambiguated with a 64-bit
/// FNV-1a of the full input. Note the weaker guarantee there: encoding is
/// injective, but any fixed-width digest of unbounded input cannot be, so two
/// names sharing a 200-character encoded prefix *and* a hash would collide.
/// That needs on the order of 2^32 such names to become likely, which is not a
/// concern for names a host chooses. It is not a cryptographic guarantee: if
/// session names are attacker-controlled, hash them yourself before passing
/// them here.
fn encode_segment(name: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        {
            out.push(byte as char);
        } else {
            // Uppercase hex only, so an escape never varies by case either.
            let _ = write!(out, "%{byte:02X}");
        }
    }
    if out.is_empty() {
        // Only the empty name reaches here, and it needs a segment no other
        // input can produce. A bare `%` qualifies: every literal `%` escapes to
        // `%25`, so no non-empty name ever encodes to it. Mapping empty to a
        // word like "unnamed" would collide with the literal name `unnamed`.
        return "%".into();
    }
    if out.len() > MAX_STEM {
        // Cut between encoded units so a half-written `%4` is never emitted.
        let mut cut = MAX_STEM;
        while cut > 0 && !is_encoding_boundary(&out, cut) {
            cut -= 1;
        }
        return format!("{}-{:016x}", &out[..cut], fnv1a(name.as_bytes()));
    }
    out
}

/// Whether `at` splits `s` between encoded units rather than inside a `%XX`.
fn is_encoding_boundary(s: &str, at: usize) -> bool {
    let b = s.as_bytes();
    !((at >= 1 && b[at - 1] == b'%') || (at >= 2 && b[at - 2] == b'%'))
}

/// FNV-1a, 64-bit. Chosen over [`std::hash::DefaultHasher`], whose algorithm is
/// explicitly allowed to change between Rust releases: that would silently
/// repoint every stored session on a toolchain upgrade. This is fixed forever.
/// It is not cryptographic and does not need to be, since it only disambiguates
/// names the caller chose.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A project path reduced to one path segment, so sessions partition by project
/// without nesting the whole absolute path.
fn project_slug(project: &Path) -> String {
    encode_segment(&project.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store in a unique temp directory, plus the project path to use.
    fn store(tag: &str) -> (SessionStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "agent-abstraction-{tag}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        (SessionStore::open(dir), PathBuf::from("/home/me/proj"))
    }

    #[test]
    fn names_and_projects_reduce_to_one_safe_segment() {
        // Readable names pass through untouched, which is the point of encoding
        // only what has to be encoded.
        assert_eq!(encode_segment("greet-flow"), "greet-flow");
        assert_eq!(encode_segment("v1.2_final"), "v1.2_final");
        // The empty name gets a segment no other input can produce. Mapping it
        // to a word would collide with a caller who literally used that word.
        assert_eq!(encode_segment(""), "%");
        assert_ne!(encode_segment(""), encode_segment("unnamed"));

        // `.` stays literal for readability, so traversal safety rests entirely
        // on the separator being encoded. A `..` embedded in a segment is inert.
        for name in ["../../etc/passwd", "..", ".", "a/b", "a\\b"] {
            let encoded = encode_segment(name);
            assert!(!encoded.contains('/'), "{name:?} kept a separator");
            assert!(!encoded.contains('\\'), "{name:?} kept a separator");
            assert!(
                Path::new(&encoded).components().count() == 1,
                "{name:?} encoded to more than one component"
            );
        }
        assert!(!project_slug(Path::new("/home/me/My Proj")).contains('/'));
    }

    /// The property that matters: distinct names never share a file. The old
    /// fold-to-dash scheme mapped `café` and `cafe-` together, so the second
    /// session to use that name silently resumed the first one's conversation.
    #[test]
    fn distinct_names_never_share_an_encoded_segment() {
        let names = [
            "café",
            "cafe-",
            "cafe",
            "Chat",
            "chat",
            "CHAT",
            "a/b",
            "a-b",
            "a b",
            "..",
            "%41",
            "A",
            "",
            "unnamed",
            "日本語",
            "🙂",
        ];
        let mut seen = std::collections::HashMap::new();
        for name in names {
            // Compare case-insensitively: macOS and Windows would treat two
            // segments differing only by case as the same file.
            let key = encode_segment(name).to_ascii_lowercase();
            if let Some(previous) = seen.insert(key.clone(), name) {
                panic!("{name:?} and {previous:?} both encode to {key:?}");
            }
        }
    }

    #[test]
    fn a_very_long_name_stays_within_filename_limits_and_stays_unique() {
        let a = "x".repeat(5_000);
        let b = format!("{a}different");
        let (ea, eb) = (encode_segment(&a), encode_segment(&b));

        // Room for the `.json` extension under a 255-byte filename cap.
        assert!(ea.len() < 250, "{}", ea.len());
        assert!(eb.len() < 250);
        assert_ne!(ea, eb, "truncation must not collapse distinct names");
    }

    #[test]
    fn truncation_never_splits_an_escape_sequence() {
        // All-uppercase encodes to three bytes per character, forcing the cut.
        let encoded = encode_segment(&"A".repeat(2_000));
        let stem = encoded.rsplit_once('-').unwrap().0;
        // Every `%` in the stem must still be followed by two hex digits.
        for (i, _) in stem.match_indices('%') {
            assert!(i + 2 < stem.len(), "escape split at {i} in {stem:?}");
        }
    }

    /// The record keeps the caller's name verbatim, so a listing can hand back
    /// what they actually passed rather than a mangled path segment.
    #[test]
    fn the_record_preserves_the_original_name() {
        let (store, project) = store("original-name");
        store
            .bind(Agent::Claude, &project, "Greet Flow ☕", "t-1")
            .unwrap();
        let record = store.get(&project, "Greet Flow ☕").unwrap().unwrap();
        assert_eq!(record.name, "Greet Flow ☕");
        assert_eq!(store.list(&project).unwrap()[0].name, "Greet Flow ☕");
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn a_path_traversing_name_cannot_escape_the_store() {
        let (store, project) = store("escape");
        for name in ["../../etc/passwd", "..", "/etc/passwd", "a/../../b"] {
            let path = store.path_of(&project, name);
            assert!(path.starts_with(&store.dir), "{name:?} escaped to {path:?}");
            // The whole name has to land in exactly one filename, so no part of
            // it can be reinterpreted as a directory step.
            assert_eq!(
                path.strip_prefix(&store.dir).unwrap().components().count(),
                2,
                "{name:?} produced extra path components: {path:?}"
            );
        }
    }

    #[test]
    fn a_missing_session_plans_a_create() {
        let (store, project) = store("create");
        let (phase, cont) = store.plan(Agent::Claude, &project, "chat", false).unwrap();
        assert_eq!(phase, Phase::Create);
        // Claude mints, so the id exists before the process does.
        let Continue::NewWith(id) = cont else {
            panic!("a minting agent must allocate an id up front, got {cont:?}")
        };
        assert!(Uuid::parse_str(&id).is_ok(), "{id} must be a UUID");
    }

    #[test]
    fn a_printing_agent_starts_without_an_id() {
        let (store, project) = store("printed");
        let (phase, cont) = store.plan(Agent::Codex, &project, "chat", false).unwrap();
        assert_eq!(phase, Phase::Create);
        assert_eq!(cont, Continue::New, "codex's id only exists once printed");
    }

    #[test]
    fn a_bound_session_plans_a_continue_and_survives_a_round_trip() {
        let (store, project) = store("continue");
        store
            .bind(Agent::Claude, &project, "chat", "sess-1")
            .unwrap();

        let (phase, cont) = store.plan(Agent::Claude, &project, "chat", false).unwrap();
        assert_eq!(phase, Phase::Continue);
        assert_eq!(cont, Continue::Resume("sess-1".into()));

        let record = store.get(&project, "chat").unwrap().unwrap();
        assert_eq!(record.token, "sess-1");
        assert_eq!(record.agent, Agent::Claude);
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn rebinding_refreshes_the_token_but_keeps_the_creation_time() {
        let (store, project) = store("rebind");
        let first = store
            .bind(Agent::Claude, &project, "chat", "sess-1")
            .unwrap();
        let second = store
            .bind(Agent::Claude, &project, "chat", "sess-2")
            .unwrap();
        assert_eq!(second.token, "sess-2");
        assert_eq!(second.created, first.created);
        assert!(second.updated >= first.updated);
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn a_session_cannot_migrate_between_agents() {
        let (store, project) = store("conflict");
        store
            .bind(Agent::Claude, &project, "chat", "sess-1")
            .unwrap();
        let err = store
            .plan(Agent::Codex, &project, "chat", false)
            .unwrap_err();
        assert!(
            matches!(err, Error::SessionConflict { bound, requested, .. }
                if bound == Agent::Claude && requested == Agent::Codex),
            "got {err:?}"
        );
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn forking_is_refused_by_agents_that_cannot_fork() {
        let (store, project) = store("fork");
        store.bind(Agent::Codex, &project, "chat", "t-1").unwrap();
        assert!(matches!(
            store.plan(Agent::Codex, &project, "chat", true),
            Err(Error::Unsupported { .. })
        ));

        store.bind(Agent::Claude, &project, "c2", "sess-1").unwrap();
        let (phase, cont) = store.plan(Agent::Claude, &project, "c2", true).unwrap();
        assert_eq!(phase, Phase::Fork);
        assert_eq!(cont, Continue::Fork("sess-1".into()));
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn forking_a_session_that_does_not_exist_yet_just_creates_one() {
        let (store, project) = store("fork-new");
        let (phase, _) = store.plan(Agent::Claude, &project, "fresh", true).unwrap();
        assert_eq!(phase, Phase::Create, "nothing to branch from yet");
    }

    #[test]
    fn a_corrupt_record_is_reported_rather_than_silently_ignored() {
        let (store, project) = store("corrupt");
        let path = store.path_of(&project, "chat");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not json").unwrap();
        // Treating this as "no session" would silently abandon a conversation
        // the caller believes they are still in.
        assert!(matches!(
            store.get(&project, "chat"),
            Err(Error::Store { .. })
        ));
        assert!(matches!(
            store.plan(Agent::Claude, &project, "chat", false),
            Err(Error::Store { .. })
        ));
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn sessions_list_per_project_and_forgetting_is_idempotent() {
        let (store, project) = store("list");
        store.bind(Agent::Claude, &project, "a", "t-a").unwrap();
        store.bind(Agent::Claude, &project, "b", "t-b").unwrap();
        let mut names: Vec<_> = store
            .list(&project)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        names.sort();
        assert_eq!(names, ["a", "b"]);

        store.forget(&project, "a").unwrap();
        assert!(store.get(&project, "a").unwrap().is_none());
        // Forgetting twice is not an error.
        store.forget(&project, "a").unwrap();
        assert_eq!(store.list(&project).unwrap().len(), 1);
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn the_same_name_in_two_projects_does_not_collide() {
        let (store, project) = store("projects");
        let other = PathBuf::from("/home/me/other");
        store.bind(Agent::Claude, &project, "chat", "t-1").unwrap();
        store.bind(Agent::Claude, &other, "chat", "t-2").unwrap();
        assert_eq!(store.get(&project, "chat").unwrap().unwrap().token, "t-1");
        assert_eq!(store.get(&other, "chat").unwrap().unwrap().token, "t-2");
        fs::remove_dir_all(&store.dir).ok();
    }
}
