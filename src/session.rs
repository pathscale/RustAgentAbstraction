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
pub struct SessionRecord {
    /// The caller's stable name, as stored (sanitized for a path).
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
    #[must_use]
    pub fn path_of(&self, project: &Path, name: &str) -> PathBuf {
        self.dir
            .join(project_slug(project))
            .join(format!("{}.json", sanitize(name)))
    }

    /// The stored record, or `None` when absent.
    ///
    /// A corrupt record reads as absent: the next turn starts a fresh
    /// conversation, which is recoverable, rather than failing the run over a
    /// cache the caller never asked about.
    #[must_use]
    pub fn get(&self, project: &Path, name: &str) -> Option<SessionRecord> {
        let text = fs::read_to_string(self.path_of(project, name)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Every session recorded for `project`, in unspecified order.
    #[must_use]
    pub fn list(&self, project: &Path) -> Vec<SessionRecord> {
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

    /// Decide how `name` continues, and produce the [`Continue`] to run with.
    ///
    /// For a minting agent with no prior record this allocates the id here and
    /// now, so the caller can persist the binding before spawning.
    ///
    /// # Errors
    /// [`Error::SessionConflict`] when the name already belongs to another
    /// agent; [`Error::Unsupported`] when `fork` is asked of an agent that
    /// cannot fork, or when the agent exposes no session id at all.
    pub fn plan(
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
        let existing = self.get(project, name);
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
        let now = now_secs();
        let record = SessionRecord {
            name: sanitize(name),
            project: project.display().to_string(),
            agent,
            token: token.to_string(),
            created: self.get(project, name).map_or(now, |r| r.created),
            updated: now,
        };

        let path = self.path_of(project, name);
        let store_err = |source| Error::Store {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(store_err)?;
        }
        let mut text = serde_json::to_string_pretty(&record)
            .map_err(|e| store_err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        text.push('\n');
        // Write beside the target and rename, so a reader never observes a
        // partial record.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(store_err)?;
        fs::rename(&tmp, &path).map_err(store_err)?;
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

/// Seconds since the epoch. A pre-1970 clock reads as 0 rather than panicking;
/// these timestamps are for display, not for correctness.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Reduce an arbitrary name to one safe path segment.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_ascii_lowercase();
    // Collapse runs of separators so `a//b` and `a-b` do not both appear.
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    if out.is_empty() {
        "unnamed".into()
    } else {
        out
    }
}

/// A directory path reduced to one path segment, so sessions partition by
/// project without nesting the whole absolute path.
fn project_slug(project: &Path) -> String {
    sanitize(&project.display().to_string())
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
        assert_eq!(sanitize("Greet Flow"), "greet-flow");
        assert_eq!(sanitize("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize("!!!"), "unnamed");
        assert_eq!(
            project_slug(Path::new("/home/me/My Proj")),
            "home-me-my-proj"
        );
    }

    #[test]
    fn a_path_traversing_name_cannot_escape_the_store() {
        let (store, project) = store("escape");
        let path = store.path_of(&project, "../../etc/passwd");
        assert_eq!(path.file_name().unwrap(), "etc-passwd.json");
        assert!(path.starts_with(&store.dir));
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

        let record = store.get(&project, "chat").unwrap();
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
    fn a_corrupt_record_reads_as_absent_rather_than_failing() {
        let (store, project) = store("corrupt");
        let path = store.path_of(&project, "chat");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not json").unwrap();
        assert!(store.get(&project, "chat").is_none());
        assert_eq!(
            store
                .plan(Agent::Claude, &project, "chat", false)
                .unwrap()
                .0,
            Phase::Create
        );
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn sessions_list_per_project_and_forgetting_is_idempotent() {
        let (store, project) = store("list");
        store.bind(Agent::Claude, &project, "a", "t-a").unwrap();
        store.bind(Agent::Claude, &project, "b", "t-b").unwrap();
        let mut names: Vec<_> = store.list(&project).into_iter().map(|r| r.name).collect();
        names.sort();
        assert_eq!(names, ["a", "b"]);

        store.forget(&project, "a").unwrap();
        assert!(store.get(&project, "a").is_none());
        // Forgetting twice is not an error.
        store.forget(&project, "a").unwrap();
        assert_eq!(store.list(&project).len(), 1);
        fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn the_same_name_in_two_projects_does_not_collide() {
        let (store, project) = store("projects");
        let other = PathBuf::from("/home/me/other");
        store.bind(Agent::Claude, &project, "chat", "t-1").unwrap();
        store.bind(Agent::Claude, &other, "chat", "t-2").unwrap();
        assert_eq!(store.get(&project, "chat").unwrap().token, "t-1");
        assert_eq!(store.get(&other, "chat").unwrap().token, "t-2");
        fs::remove_dir_all(&store.dir).ok();
    }
}
