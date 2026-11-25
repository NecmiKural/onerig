//! Chat session persistence on disk (design sections 5 and 8):
//! `<repo>/.onerig/sessions/<session id>/transcript.jsonl`, `lock`, and `raw-<n>.log`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::{Lock, LockError, Phase, RunDir, StoreError, sorted_entries};
use crate::turn::{ChatMessage, Proposal};

/// `<repo>/.onerig/sessions`.
pub fn sessions_root(repo: &Path) -> PathBuf {
    repo.join(".onerig").join("sessions")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDir {
    path: PathBuf,
}

impl SessionDir {
    pub fn create(repo: &Path, session_id: &str) -> Result<SessionDir, StoreError> {
        let root = sessions_root(repo);
        std::fs::create_dir_all(&root).map_err(|e| io_err(&root, e))?;
        let path = root.join(session_id);
        match std::fs::create_dir(&path) {
            Ok(()) => Ok(SessionDir { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(StoreError::Exists(session_id.to_string()))
            }
            Err(e) => Err(io_err(&path, e)),
        }
    }

    pub fn open(repo: &Path, session_id: &str) -> Result<SessionDir, StoreError> {
        let path = sessions_root(repo).join(session_id);
        if path.is_dir() {
            Ok(SessionDir { path })
        } else {
            Err(StoreError::NoRun(session_id.to_string()))
        }
    }

    pub fn latest(repo: &Path) -> Result<Option<String>, StoreError> {
        let ids = sorted_entries(&sessions_root(repo))?;
        Ok(ids
            .into_iter()
            .find(|id| sessions_root(repo).join(id).is_dir()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lock(&self) -> Result<Lock, LockError> {
        Lock::acquire_file(self.path.join("lock"))
    }

    pub fn raw_log_path(&self, seq: u64) -> PathBuf {
        self.path.join(format!("raw-{seq}.log"))
    }

    /// Scans `raw-*.log` files in the session directory and returns the next sequence number.
    pub fn next_raw_log_seq(&self) -> u64 {
        let max = std::fs::read_dir(&self.path)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|name| {
                name.strip_prefix("raw-")?
                    .strip_suffix(".log")?
                    .parse::<u64>()
                    .ok()
            })
            .max();
        max.map_or(1, |n| n + 1)
    }

    pub fn append(&self, record: &TranscriptRecord) -> Result<(), StoreError> {
        let path = self.transcript_path();
        let line = serde_json::to_string(record).map_err(|e| fmt_err(&path, e))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| io_err(&path, e))?;
        writeln!(file, "{line}").map_err(|e| io_err(&path, e))
    }

    pub fn read_transcript(&self) -> Result<Vec<TranscriptRecord>, StoreError> {
        let path = self.transcript_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&path, e)),
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(|e| fmt_err(&path, e)))
            .collect()
    }

    fn transcript_path(&self) -> PathBuf {
        self.path.join("transcript.jsonl")
    }
}

fn io_err(path: &Path, error: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn fmt_err(path: &Path, error: impl std::fmt::Display) -> StoreError {
    StoreError::Format {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRecord {
    pub ts: u64,
    #[serde(flatten)]
    pub entry: TranscriptEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    Owner {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        btw: bool,
    },
    Planner {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<Proposal>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        btw: bool,
    },
    Run {
        run: String,
        #[serde(default)]
        from_owner: bool,
    },
    System {
        text: String,
    },
}

/// Extracts owner and planner messages from transcript records in chronological order.
pub fn messages(records: &[TranscriptRecord]) -> Vec<ChatMessage> {
    records
        .iter()
        .filter_map(|record| match &record.entry {
            TranscriptEntry::Owner { text, .. } => Some(ChatMessage {
                from_owner: true,
                text: text.clone(),
            }),
            TranscriptEntry::Planner { text, .. } => Some(ChatMessage {
                from_owner: false,
                text: text.clone(),
            }),
            TranscriptEntry::Run { .. } | TranscriptEntry::System { .. } => None,
        })
        .collect()
}

/// Reconstructs the current unapproved waiting plan from transcript records.
pub fn waiting_plan(records: &[TranscriptRecord]) -> Option<Proposal> {
    let mut waiting = None;
    for record in records {
        match &record.entry {
            TranscriptEntry::Planner {
                plan: Some(proposal),
                btw: false,
                ..
            } => waiting = Some(proposal.clone()),
            TranscriptEntry::Run {
                from_owner: false, ..
            } => waiting = None,
            _ => {}
        }
    }
    waiting
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRuns {
    pub latest_done_branch: Option<String>,
    pub planner_cwd: PathBuf,
    pub latest_resumable: Option<String>,
}

/// Inspects the runs listed in `records` to find the latest done base and resumable run.
pub fn session_runs(repo: &Path, records: &[TranscriptRecord]) -> SessionRuns {
    let mut result = SessionRuns {
        latest_done_branch: None,
        planner_cwd: repo.to_path_buf(),
        latest_resumable: None,
    };
    for record in records {
        let TranscriptEntry::Run { run, .. } = &record.entry else {
            continue;
        };
        let Ok(state) = RunDir::open(repo, run).and_then(|d| d.read_state()) else {
            continue;
        };
        if state.phase == Phase::Done {
            result.latest_done_branch = Some(state.branch);
            result.planner_cwd = if state.worktree.is_dir() {
                state.worktree
            } else {
                repo.to_path_buf()
            };
        } else if state.phase != Phase::Failed {
            result.latest_resumable = Some(run.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Plan, Task};
    use crate::store::{LaneMark, STATE_VERSION, State};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn proposal(goal: &str) -> Proposal {
        Proposal {
            goal: goal.into(),
            plan: Plan {
                tasks: vec![Task {
                    id: "t1".into(),
                    title: "Task 1".into(),
                    instructions: "Do it.".into(),
                    acceptance: vec!["Done".into()],
                    lane: None,
                    from_owner: false,
                }],
            },
        }
    }

    #[test]
    fn transcript_records_match_documented_json_and_round_trip() {
        let records = [
            TranscriptRecord {
                ts: 1,
                entry: TranscriptEntry::Owner {
                    text: "add retries".into(),
                    btw: false,
                },
            },
            TranscriptRecord {
                ts: 2,
                entry: TranscriptEntry::Planner {
                    text: "Two tasks".into(),
                    plan: Some(proposal("Add retries")),
                    btw: false,
                },
            },
            TranscriptRecord {
                ts: 3,
                entry: TranscriptEntry::Run {
                    run: "r1".into(),
                    from_owner: false,
                },
            },
            TranscriptRecord {
                ts: 4,
                entry: TranscriptEntry::System {
                    text: "paused".into(),
                },
            },
        ];

        assert_eq!(
            serde_json::to_value(&records[0]).unwrap(),
            json!({"ts": 1, "kind": "owner", "text": "add retries"})
        );
        assert_eq!(
            serde_json::to_value(&records[2]).unwrap(),
            json!({"ts": 3, "kind": "run", "run": "r1", "from_owner": false})
        );

        let repo = tempfile::tempdir().unwrap();
        let dir = SessionDir::create(repo.path(), "s1").unwrap();
        for rec in &records {
            dir.append(rec).unwrap();
        }
        assert_eq!(dir.read_transcript().unwrap(), records);
    }

    #[test]
    fn session_dir_handles_latest_lock_and_raw_log_sequence() {
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(SessionDir::latest(repo.path()).unwrap(), None);

        let s1 = SessionDir::create(repo.path(), "20261125-090000-0001").unwrap();
        let s2 = SessionDir::create(repo.path(), "20261125-100000-0002").unwrap();
        assert_eq!(
            SessionDir::latest(repo.path()).unwrap().as_deref(),
            Some("20261125-100000-0002")
        );
        assert_eq!(
            SessionDir::open(repo.path(), "20261125-090000-0001").unwrap(),
            s1
        );

        let _lock = s2.lock().unwrap();
        assert!(matches!(s2.lock(), Err(LockError::Held { .. })));

        assert_eq!(s2.next_raw_log_seq(), 1);
        std::fs::write(s2.raw_log_path(1), "a").unwrap();
        std::fs::write(s2.raw_log_path(4), "b").unwrap();
        assert_eq!(s2.next_raw_log_seq(), 5);
    }

    #[test]
    fn messages_waiting_plan_and_session_runs_derive_state_from_transcript() {
        let repo = tempfile::tempdir().unwrap();
        let wt = repo.path().join("wt-done");
        std::fs::create_dir(&wt).unwrap();
        let make_state = |id: &str, phase: Phase, worktree: PathBuf| State {
            v: STATE_VERSION,
            run_id: id.into(),
            goal: "g".into(),
            updated_at: 1,
            repo: repo.path().to_path_buf(),
            worktree,
            branch: format!("onerig/{id}"),
            phase,
            paused: None,
            task_index: None,
            task_base: None,
            attempt: None,
            attempts_used: 0,
            lane: None,
            unknown_streak: 0,
            skipped: vec![],
            last_end: None,
            approved: false,
            issues: vec![],
            last_output_tail: String::new(),
            job_seq: 0,
            lanes: BTreeMap::<String, LaneMark>::new(),
        };
        RunDir::create(repo.path(), "r1")
            .unwrap()
            .write_state(&make_state("r1", Phase::Done, wt.clone()))
            .unwrap();
        RunDir::create(repo.path(), "r2")
            .unwrap()
            .write_state(&make_state(
                "r2",
                Phase::Paused,
                repo.path().join("missing"),
            ))
            .unwrap();

        let recs = [
            TranscriptRecord {
                ts: 1,
                entry: TranscriptEntry::Owner {
                    text: "q".into(),
                    btw: false,
                },
            },
            TranscriptRecord {
                ts: 2,
                entry: TranscriptEntry::Planner {
                    text: "p1".into(),
                    plan: Some(proposal("G1")),
                    btw: false,
                },
            },
            TranscriptRecord {
                ts: 3,
                entry: TranscriptEntry::Run {
                    run: "r1".into(),
                    from_owner: true,
                },
            },
            TranscriptRecord {
                ts: 4,
                entry: TranscriptEntry::Planner {
                    text: "answer only".into(),
                    plan: None,
                    btw: false,
                },
            },
        ];

        assert_eq!(
            messages(&recs),
            [
                ChatMessage {
                    from_owner: true,
                    text: "q".into()
                },
                ChatMessage {
                    from_owner: false,
                    text: "p1".into()
                },
                ChatMessage {
                    from_owner: false,
                    text: "answer only".into()
                },
            ]
        );
        assert_eq!(waiting_plan(&recs), Some(proposal("G1")));

        let started = [
            recs[1].clone(),
            TranscriptRecord {
                ts: 5,
                entry: TranscriptEntry::Run {
                    run: "r2".into(),
                    from_owner: false,
                },
            },
        ];
        assert_eq!(waiting_plan(&started), None);

        let all_runs = [
            TranscriptRecord {
                ts: 3,
                entry: TranscriptEntry::Run {
                    run: "r1".into(),
                    from_owner: false,
                },
            },
            TranscriptRecord {
                ts: 5,
                entry: TranscriptEntry::Run {
                    run: "r2".into(),
                    from_owner: false,
                },
            },
        ];
        let runs = session_runs(repo.path(), &all_runs);
        assert_eq!(runs.latest_done_branch.as_deref(), Some("onerig/r1"));
        assert_eq!(runs.planner_cwd, wt);
        assert_eq!(runs.latest_resumable.as_deref(), Some("r2"));
    }
}
