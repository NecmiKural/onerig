//! What a run keeps on disk (design section 12): its state, its plan, its log
//! and its lock, plus the run id and time formats.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::plan::{Issue, Plan, VerdictKind};
use crate::runner::{HarnessEvent, Outcome};

/// Version of the `state.json` and `plan.json` format other tools may read.
/// Within one version fields are only added.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub v: u32,
    pub run_id: String,
    pub goal: String,
    /// Unix milliseconds; set on every write.
    pub updated_at: u64,
    /// The main checkout.
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<Paused>,
    /// 1-based position in the plan's tasks; `None` before the first task.
    pub task_index: Option<usize>,
    /// The worktree's `HEAD` when the current task started.
    pub task_base: Option<String>,
    /// The task's running attempt number, 1-based; never reset by resume.
    pub attempt: Option<u32>,
    /// What the attempt budget counts; resume and a new task set it to 0.
    pub attempts_used: u32,
    pub lane: Option<String>,
    pub unknown_streak: u32,
    /// Lanes skipped for the current task.
    pub skipped: Vec<String>,
    pub last_end: Option<LastEnd>,
    /// True between an approve verdict and the task commit.
    pub approved: bool,
    pub issues: Vec<Issue>,
    pub last_output_tail: String,
    /// Numbers every job of the run; names the raw log files.
    pub job_seq: u64,
    pub lanes: BTreeMap<String, LaneMark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Planning,
    Executing,
    Reviewing,
    Paused,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    pub reason: PauseReason,
    pub resume_phase: Phase,
    pub message: String,
    /// Earliest known reset time, unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    NoLane,
    Handover,
    Quota,
    Auth,
    AttemptsExhausted,
    JobFailed,
    GitError,
    Interrupted,
}

/// How the last worker attempt ended; gives the next hand-over its reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastEnd {
    Quota,
    Timeout,
    Failed,
    Interrupted,
    Revise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaneMark {
    #[serde(default)]
    pub exhausted: bool,
    /// Unix milliseconds; `None` with `exhausted` means until the next resume.
    #[serde(default)]
    pub exhausted_until: Option<u64>,
}

/// One line of `log.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    /// Unix milliseconds.
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(flatten)]
    pub entry: LogEntry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum LogEntry {
    State(StateEvent),
    Policy(PolicyEvent),
    Harness(HarnessEvent),
    Outcome(Outcome),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StateEvent {
    /// Written when a run starts or resumes, so a reader of the log knows it.
    Run {
        run_id: String,
        goal: String,
        branch: String,
    },
    Plan {
        tasks: usize,
        #[serde(default)]
        titles: Vec<String>,
    },
    Phase {
        phase: Phase,
    },
    Verdict {
        verdict: VerdictKind,
        summary: String,
        issues: Vec<Issue>,
    },
    Commit {
        hash: String,
        title: String,
    },
    Paused {
        reason: PauseReason,
        message: String,
    },
    Done {
        branch: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PolicyEvent {
    Probe {
        lane: String,
        ok: bool,
        message: String,
    },
    LaneSelected {
        lane: String,
        manual: bool,
    },
    LaneExhausted {
        lane: String,
        until: Option<u64>,
    },
    LaneSkipped {
        lane: String,
    },
    Handover {
        path: PathBuf,
    },
}

const MS_PER_DAY: u64 = 86_400_000;

/// `YYYYMMDD-HHMMSS-<4 hex>` in UTC. The hex digits mix the clock and the
/// process id so two runs started in the same second differ.
pub fn run_id(now_ms: u64, pid: u32) -> String {
    let (year, month, day) = civil_from_days(now_ms / MS_PER_DAY);
    let mix = ((now_ms % 1000) as u32).wrapping_mul(2_654_435_761) ^ pid;
    format!(
        "{year:04}{month:02}{day:02}-{}-{:04x}",
        format_time(now_ms).replace(':', ""),
        mix & 0xffff
    )
}

/// `HH:MM:SS` in UTC.
pub fn format_time(now_ms: u64) -> String {
    let seconds = (now_ms % MS_PER_DAY) / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`, for non-negative days).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("there is no run {0} in this repository")]
    NoRun(String),
    #[error("run {0} already exists")]
    Exists(String),
    #[error("{}: {message}", path.display())]
    Io { path: PathBuf, message: String },
    #[error("{} is not a file onerig can read: {message}", path.display())]
    Format { path: PathBuf, message: String },
    #[error("{} has format version {found}, which this onerig does not know; upgrade onerig", path.display())]
    Version { path: PathBuf, found: u64 },
}

fn io_error(path: &Path, error: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn format_error(path: &Path, error: impl std::fmt::Display) -> StoreError {
    StoreError::Format {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

/// `<repo>/.onerig/runs`.
pub fn runs_root(repo: &Path) -> PathBuf {
    repo.join(".onerig").join("runs")
}

/// One run's directory, `<repo>/.onerig/runs/<run id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    pub fn create(repo: &Path, run_id: &str) -> Result<RunDir, StoreError> {
        let root = runs_root(repo);
        std::fs::create_dir_all(&root).map_err(|e| io_error(&root, e))?;
        let path = root.join(run_id);
        match std::fs::create_dir(&path) {
            Ok(()) => Ok(RunDir { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(StoreError::Exists(run_id.to_string()))
            }
            Err(e) => Err(io_error(&path, e)),
        }
    }

    pub fn open(repo: &Path, run_id: &str) -> Result<RunDir, StoreError> {
        let path = runs_root(repo).join(run_id);
        if path.is_dir() {
            Ok(RunDir { path })
        } else {
            Err(StoreError::NoRun(run_id.to_string()))
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn handover_path(&self, task_index: usize, attempt: u32) -> PathBuf {
        self.path
            .join(format!("handover-{task_index}-{attempt}.md"))
    }

    pub fn raw_log_path(&self, job_seq: u64) -> PathBuf {
        self.path.join(format!("raw-{job_seq}.log"))
    }

    fn state_path(&self) -> PathBuf {
        self.path.join("state.json")
    }

    fn plan_path(&self) -> PathBuf {
        self.path.join("plan.json")
    }

    /// Replaces `state.json` atomically: a reader sees the old or the new file.
    pub fn write_state(&self, state: &State) -> Result<(), StoreError> {
        write_atomically(&self.state_path(), state)
    }

    pub fn read_state(&self) -> Result<State, StoreError> {
        let path = self.state_path();
        let value: serde_json::Value = read_json(&path)?;
        let found = value
            .get("v")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if found != u64::from(STATE_VERSION) {
            return Err(StoreError::Version { path, found });
        }
        serde_json::from_value(value).map_err(|e| format_error(&path, e))
    }

    pub fn write_plan(&self, plan: &Plan) -> Result<(), StoreError> {
        write_atomically(&self.plan_path(), plan)
    }

    pub fn read_plan(&self) -> Result<Plan, StoreError> {
        read_json(&self.plan_path())
    }

    pub fn append_log(&self, record: &LogRecord) -> Result<(), StoreError> {
        let path = self.path.join("log.jsonl");
        let line = serde_json::to_string(record).map_err(|e| format_error(&path, e))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        writeln!(file, "{line}").map_err(|e| io_error(&path, e))
    }
}

fn write_atomically(path: &Path, value: &impl Serialize) -> Result<(), StoreError> {
    let text = serde_json::to_string_pretty(value).map_err(|e| format_error(path, e))?;
    let temporary = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        let mut file = File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    };
    write().map_err(|e| io_error(path, e))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| io_error(path, e))?;
    serde_json::from_str(&text).map_err(|e| format_error(path, e))
}

/// The greatest run id whose phase is neither done nor failed. Runs that
/// cannot be read are skipped.
pub fn latest_resumable(repo: &Path) -> Result<Option<String>, StoreError> {
    let root = runs_root(repo);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(&root, e)),
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(ids.into_iter().find(|id| is_resumable(repo, id)))
}

fn is_resumable(repo: &Path, run_id: &str) -> bool {
    RunDir::open(repo, run_id)
        .and_then(|dir| dir.read_state())
        .is_ok_and(|state| !matches!(state.phase, Phase::Done | Phase::Failed))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LockError {
    #[error("another onerig process is working on this run (lock {})", path.display())]
    Held { path: PathBuf },
    #[error(
        "process group {pgid}, started by an earlier onerig process for this run, still exists \
         and may still be editing the worktree; stop it and try again (lock {})",
        path.display()
    )]
    OrphanGroup { pgid: i32, path: PathBuf },
    #[error("cannot use the lock file {}: {message}", path.display())]
    Io { path: PathBuf, message: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct LockContent {
    pid: u32,
    pgid: Option<i32>,
}

/// Exclusive hold on a run, released by the kernel however the process ends.
#[derive(Debug)]
pub struct Lock {
    file: File,
    path: PathBuf,
}

impl Lock {
    pub fn acquire(run_dir: &RunDir) -> Result<Lock, LockError> {
        let path = run_dir.path().join("lock");
        let lock_io = |e: std::io::Error| LockError::Io {
            path: path.clone(),
            message: e.to_string(),
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(lock_io)?;
        if !try_flock(&file) {
            return Err(LockError::Held { path });
        }
        let mut lock = Lock { file, path };
        if let Some(pgid) = lock.previous_pgid().filter(|&pgid| group_exists(pgid)) {
            return Err(LockError::OrphanGroup {
                pgid,
                path: lock.path,
            });
        }
        lock.set_pgid(None)?;
        Ok(lock)
    }

    /// Records the running child's process group, or `None` once it is reaped.
    pub fn set_pgid(&mut self, pgid: Option<i32>) -> Result<(), LockError> {
        let content = LockContent {
            pid: std::process::id(),
            pgid,
        };
        let text = serde_json::to_string(&content).unwrap_or_default();
        let mut write = || -> std::io::Result<()> {
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(text.as_bytes())?;
            self.file.set_len(text.len() as u64)?;
            self.file.sync_data()
        };
        write().map_err(|e| LockError::Io {
            path: self.path.clone(),
            message: e.to_string(),
        })
    }

    fn previous_pgid(&mut self) -> Option<i32> {
        let mut text = String::new();
        self.file.read_to_string(&mut text).ok()?;
        serde_json::from_str::<LockContent>(&text).ok()?.pgid
    }
}

fn try_flock(file: &File) -> bool {
    // SAFETY: the descriptor belongs to `file`, which outlives this call.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Signal 0 checks that a process group exists without touching it.
fn group_exists(pgid: i32) -> bool {
    // 0 would address this process's own group and 1 every process.
    if pgid <= 1 {
        return false;
    }
    // SAFETY: kill with signal 0 sends nothing; it only checks the target.
    let result = unsafe { libc::kill(-pgid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FailureKind;
    use serde_json::json;

    // These are the log lines the design document shows; other tools and the
    // plain output depend on them.
    #[test]
    fn log_records_have_the_documented_shape() {
        let harness = LogRecord {
            ts: 1_763_929_001_000,
            task: Some(1),
            attempt: Some(1),
            lane: Some("deepseek".into()),
            entry: LogEntry::Harness(HarnessEvent::Tool {
                name: "edit".into(),
                detail: "src/http.rs".into(),
            }),
        };
        let policy = LogRecord {
            ts: 1_763_929_090_001,
            task: Some(1),
            attempt: None,
            lane: None,
            entry: LogEntry::Policy(PolicyEvent::LaneExhausted {
                lane: "deepseek".into(),
                until: None,
            }),
        };

        assert_eq!(
            serde_json::to_value(&harness).unwrap(),
            json!({ "ts": 1_763_929_001_000u64, "task": 1, "attempt": 1, "lane": "deepseek",
                    "kind": "harness", "data": { "type": "tool", "name": "edit", "detail": "src/http.rs" } })
        );
        assert_eq!(
            serde_json::to_value(&policy).unwrap(),
            json!({ "ts": 1_763_929_090_001u64, "task": 1,
                    "kind": "policy", "data": { "type": "lane_exhausted", "lane": "deepseek", "until": null } })
        );
    }

    #[test]
    fn a_log_record_survives_a_round_trip() {
        let record = LogRecord {
            ts: 5,
            task: Some(2),
            attempt: Some(3),
            lane: Some("gemini".into()),
            entry: LogEntry::Outcome(Outcome::Failed {
                kind: FailureKind::Quota,
                message: "empty".into(),
                reset_at: Some(9),
            }),
        };

        let line = serde_json::to_string(&record).unwrap();

        assert_eq!(serde_json::from_str::<LogRecord>(&line).unwrap(), record);
    }

    #[test]
    fn a_paused_state_reads_back_with_optional_fields_absent() {
        let text = json!({
            "v": 1, "run_id": "20261123-201500-a1f3", "goal": "g", "updated_at": 1,
            "repo": "/r", "worktree": "/w", "branch": "onerig/20261123-201500-a1f3",
            "phase": "paused",
            "paused": { "reason": "no_lane", "resume_phase": "executing", "message": "m" },
            "task_index": 2, "task_base": "3f2a", "attempt": 2, "attempts_used": 2,
            "lane": "deepseek", "unknown_streak": 0, "skipped": [], "last_end": "quota",
            "approved": false, "issues": [], "last_output_tail": "", "job_seq": 7,
            "lanes": { "deepseek": { "exhausted": true, "exhausted_until": null }, "gemini": {} }
        });

        let state: State = serde_json::from_value(text).unwrap();

        assert_eq!(state.phase, Phase::Paused);
        assert_eq!(state.paused.as_ref().unwrap().reason, PauseReason::NoLane);
        assert_eq!(state.paused.as_ref().unwrap().until, None);
        assert_eq!(state.last_end, Some(LastEnd::Quota));
        assert!(state.lanes["deepseek"].exhausted);
        assert_eq!(state.lanes["gemini"], LaneMark::default());
    }

    fn sample_state(run_id: &str, phase: Phase) -> State {
        State {
            v: STATE_VERSION,
            run_id: run_id.into(),
            goal: "g".into(),
            updated_at: 1,
            repo: "/r".into(),
            worktree: "/w".into(),
            branch: format!("onerig/{run_id}"),
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
            lanes: BTreeMap::new(),
        }
    }

    #[test]
    fn a_run_id_is_the_utc_date_and_time_plus_four_hex_digits() {
        let id = run_id(1_795_464_900_000, 4242);

        assert!(id.starts_with("20261123-201500-"), "{id}");
        assert_eq!(id.len(), "20261123-201500-a1f3".len());
        assert!(id[16..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn run_ids_get_calendar_edges_right() {
        let cases = [
            (0, "19700101-000000"),
            (1_709_210_096_000, "20240229-123456"),
            (1_767_225_599_000, "20251231-235959"),
            (1_767_225_600_000, "20260101-000000"),
            (4_107_542_400_000, "21000301-000000"),
        ];

        for (ms, expected) in cases {
            assert_eq!(&run_id(ms, 1)[..15], expected, "{ms}");
        }
    }

    #[test]
    fn run_ids_from_two_processes_in_the_same_second_differ() {
        assert_ne!(
            run_id(1_795_464_900_123, 100),
            run_id(1_795_464_900_123, 101)
        );
    }

    #[test]
    fn times_print_as_utc_clock_time() {
        assert_eq!(format_time(1_795_464_901_999), "20:15:01");
        assert_eq!(format_time(0), "00:00:00");
    }

    #[test]
    fn a_run_directory_is_created_once_and_opened_after() {
        let repo = tempfile::tempdir().unwrap();

        let created = RunDir::create(repo.path(), "r1").unwrap();

        assert!(created.path().ends_with(".onerig/runs/r1"));
        assert_eq!(
            RunDir::create(repo.path(), "r1"),
            Err(StoreError::Exists("r1".into()))
        );
        assert_eq!(RunDir::open(repo.path(), "r1").unwrap(), created);
        assert_eq!(
            RunDir::open(repo.path(), "r2"),
            Err(StoreError::NoRun("r2".into()))
        );
    }

    #[test]
    fn run_files_are_named_as_documented() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();

        assert_eq!(
            dir.handover_path(2, 3).file_name().unwrap(),
            "handover-2-3.md"
        );
        assert_eq!(dir.raw_log_path(7).file_name().unwrap(), "raw-7.log");
    }

    #[test]
    fn state_and_plan_are_written_and_read_back() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        let state = sample_state("r1", Phase::Executing);
        let plan = Plan { tasks: vec![] };

        dir.write_state(&state).unwrap();
        dir.write_plan(&plan).unwrap();

        assert_eq!(dir.read_state().unwrap(), state);
        assert_eq!(dir.read_plan().unwrap(), plan);
        assert!(!dir.path().join("state.json.tmp").exists());
    }

    #[test]
    fn a_state_of_another_version_is_refused() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        let mut value = serde_json::to_value(sample_state("r1", Phase::Done)).unwrap();
        value["v"] = json!(2);
        std::fs::write(dir.path().join("state.json"), value.to_string()).unwrap();

        assert!(matches!(
            dir.read_state(),
            Err(StoreError::Version { found: 2, .. })
        ));
    }

    #[test]
    fn a_damaged_state_is_a_format_error() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        std::fs::write(dir.path().join("state.json"), "{\"v\": 1}").unwrap();

        assert!(matches!(dir.read_state(), Err(StoreError::Format { .. })));
    }

    #[test]
    fn log_records_are_appended_one_per_line() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        let record = |tasks| LogRecord {
            ts: 1,
            task: None,
            attempt: None,
            lane: None,
            entry: LogEntry::State(StateEvent::Plan {
                tasks,
                titles: vec![],
            }),
        };

        dir.append_log(&record(1)).unwrap();
        dir.append_log(&record(2)).unwrap();

        let text = std::fs::read_to_string(dir.path().join("log.jsonl")).unwrap();
        let lines: Vec<LogRecord> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines, [record(1), record(2)]);
    }

    #[test]
    fn the_latest_resumable_run_skips_finished_and_unreadable_runs() {
        let repo = tempfile::tempdir().unwrap();
        for (id, phase) in [
            ("r1", Phase::Paused),
            ("r2", Phase::Executing),
            ("r3", Phase::Done),
            ("r5", Phase::Failed),
        ] {
            RunDir::create(repo.path(), id)
                .unwrap()
                .write_state(&sample_state(id, phase))
                .unwrap();
        }
        RunDir::create(repo.path(), "r4").unwrap();

        assert_eq!(latest_resumable(repo.path()).unwrap(), Some("r2".into()));
    }

    #[test]
    fn no_runs_means_nothing_to_resume() {
        let repo = tempfile::tempdir().unwrap();

        assert_eq!(latest_resumable(repo.path()).unwrap(), None);
    }

    fn lock_content(dir: &RunDir) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("lock")).unwrap()).unwrap()
    }

    #[test]
    fn a_held_lock_refuses_a_second_holder_until_it_is_dropped() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();

        let first = Lock::acquire(&dir).unwrap();

        assert!(matches!(Lock::acquire(&dir), Err(LockError::Held { .. })));
        drop(first);
        // Other tests fork children in parallel; a child between fork and exec
        // briefly holds a copy of the descriptor, and with it the lock.
        let reacquired = (0..200).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            Lock::acquire(&dir).is_ok()
        });
        assert!(reacquired);
    }

    #[test]
    fn the_lock_records_the_pid_and_the_childs_group() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        let mut lock = Lock::acquire(&dir).unwrap();

        assert_eq!(
            lock_content(&dir),
            json!({ "pid": std::process::id(), "pgid": null })
        );
        lock.set_pgid(Some(12345)).unwrap();
        assert_eq!(
            lock_content(&dir),
            json!({ "pid": std::process::id(), "pgid": 12345 })
        );
    }

    #[test]
    fn a_recorded_group_that_still_exists_is_reported_not_signalled() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        // SAFETY: getpgrp has no preconditions.
        let own_group = unsafe { libc::getpgrp() };
        std::fs::write(
            dir.path().join("lock"),
            json!({ "pid": 1, "pgid": own_group }).to_string(),
        )
        .unwrap();

        let result = Lock::acquire(&dir);

        assert!(matches!(result, Err(LockError::OrphanGroup { pgid, .. }) if pgid == own_group));
    }

    #[test]
    fn a_recorded_group_that_is_gone_is_cleared() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        std::fs::write(
            dir.path().join("lock"),
            json!({ "pid": 1, "pgid": 2_000_000_000 }).to_string(),
        )
        .unwrap();

        let _lock = Lock::acquire(&dir).unwrap();

        assert_eq!(lock_content(&dir)["pgid"], json!(null));
    }

    #[test]
    fn a_state_without_a_version_is_refused_as_version_zero() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        std::fs::write(dir.path().join("state.json"), "{}").unwrap();

        assert!(matches!(
            dir.read_state(),
            Err(StoreError::Version { found: 0, .. })
        ));
    }

    #[test]
    fn a_shorter_lock_content_replaces_a_longer_one_completely() {
        let repo = tempfile::tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "r1").unwrap();
        let mut lock = Lock::acquire(&dir).unwrap();

        lock.set_pgid(Some(1_234_567)).unwrap();
        lock.set_pgid(None).unwrap();

        assert_eq!(lock_content(&dir)["pgid"], json!(null));
    }

    #[test]
    fn only_a_real_process_group_counts_as_existing() {
        // SAFETY: getpgrp has no preconditions.
        let own_group = unsafe { libc::getpgrp() };
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let child_pid = child.id() as i32;

        let results = [
            group_exists(0),
            group_exists(1),
            group_exists(own_group),
            group_exists(child_pid),
        ];
        child.kill().unwrap();
        child.wait().unwrap();

        // The child shares this process's group, so no group has its pid as id.
        assert_eq!(results, [false, false, true, false]);
    }
}
