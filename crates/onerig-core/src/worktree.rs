//! Git operations of a run (design section 10): the main checkout, the run's
//! branch and worktree, the task diff and the task commit. Git is driven
//! through its executable.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    #[error("`{command}` failed: {stderr}")]
    Failed { command: String, stderr: String },
    #[error("cannot run git: {0}")]
    NotRunnable(String),
    #[error("{0}")]
    Preflight(String),
}

/// What `git add -A` followed by a diff against the task base shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDiff {
    pub stat: String,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Committed {
    /// The short hash of the task commit.
    Commit(String),
    /// The task changed nothing, so no commit was made.
    Nothing,
}

/// `$XDG_DATA_HOME` when it is an absolute path, else `~/.local/share`.
/// The caller reads the environment.
pub fn data_home(xdg_data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    match (xdg_data_home.map(Path::new), home) {
        (Some(xdg), _) if xdg.is_absolute() => Some(xdg.to_path_buf()),
        (_, Some(home)) if !home.is_empty() => Some(Path::new(home).join(".local/share")),
        _ => None,
    }
}

/// `<data home>/onerig/worktrees/<repo directory name>/<run id>`.
pub fn worktree_path(data_home: &Path, repo: &Path, run_id: &str) -> PathBuf {
    let base = data_home.join("onerig").join("worktrees");
    match repo.file_name() {
        Some(name) => base.join(name).join(run_id),
        None => base.join(run_id),
    }
}

fn first_worktree(listing: &str) -> Option<PathBuf> {
    listing
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
}

fn command_line(dir: &Path, args: &[&str]) -> String {
    format!("git -C {} {}", dir.display(), args.join(" "))
}

/// The exclude file with `.onerig/` added, or `None` when it is already there.
fn with_exclude_line(current: &str) -> Option<String> {
    if current.lines().any(|line| line.trim() == ".onerig/") {
        return None;
    }
    let separator = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    Some(format!("{current}{separator}.onerig/\n"))
}

fn read_if_present(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        other => other,
    }
}

fn write_creating_dirs(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

fn file_error(path: &Path, error: std::io::Error) -> GitError {
    GitError::Failed {
        command: format!("update {}", path.display()),
        stderr: error.to_string(),
    }
}

/// Runs git with a fixed environment. Every call names its directory and can
/// never wait for a password.
#[derive(Debug, Clone, Default)]
pub struct Git {
    envs: Vec<(String, String)>,
}

impl Git {
    pub fn new() -> Git {
        Git::default()
    }

    /// Ignores the global and system git configuration, so a developer's
    /// signing or hooks cannot leak into tests.
    #[cfg(test)]
    pub(crate) fn isolated() -> Git {
        Git {
            envs: vec![
                ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ],
        }
    }

    /// Runs `git` with the given arguments in `dir`; for test setup.
    #[cfg(test)]
    pub(crate) fn must(&self, dir: &Path, args: &[&str]) -> String {
        self.run(dir, args).unwrap_or_else(|e| panic!("{e}"))
    }

    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .envs(
                self.envs
                    .iter()
                    .map(|(k, v)| (OsStr::new(k), OsStr::new(v))),
            )
            .stdin(Stdio::null());
        command
    }

    /// Exit code and output of a git call; a git that cannot start is an error.
    fn output(&self, dir: &Path, args: &[&str]) -> Result<(Option<i32>, String, String), GitError> {
        let output = self
            .command(dir, args)
            .output()
            .map_err(|e| GitError::NotRunnable(e.to_string()))?;
        Ok((
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Result<String, GitError> {
        match self.output(dir, args)? {
            (Some(0), stdout, _) => Ok(stdout),
            (_, _, stderr) => Err(GitError::Failed {
                command: command_line(dir, args),
                stderr,
            }),
        }
    }

    /// For commands whose exit code 1 is an answer, not a failure.
    fn check(&self, dir: &Path, args: &[&str]) -> Result<bool, GitError> {
        match self.output(dir, args)? {
            (Some(0), _, _) => Ok(true),
            (Some(1), _, _) => Ok(false),
            (_, _, stderr) => Err(GitError::Failed {
                command: command_line(dir, args),
                stderr,
            }),
        }
    }

    /// The repository's main checkout, from it or from any linked worktree.
    pub fn main_checkout(&self, cwd: &Path) -> Result<PathBuf, GitError> {
        let listing = self.run(cwd, &["worktree", "list", "--porcelain"])?;
        first_worktree(&listing).ok_or_else(|| GitError::Failed {
            command: command_line(cwd, &["worktree", "list", "--porcelain"]),
            stderr: "no worktree listed".into(),
        })
    }

    /// A run needs a first commit to branch from and an identity to commit with.
    pub fn preflight(&self, main: &Path) -> Result<(), GitError> {
        if !self.check(main, &["rev-parse", "--verify", "--quiet", "HEAD"])? {
            return Err(GitError::Preflight(
                "the repository has no commits yet; make a first commit and try again".into(),
            ));
        }
        for key in ["user.name", "user.email"] {
            if !self.check(main, &["config", key])? {
                return Err(GitError::Preflight(format!(
                    "git {key} is not set; set it with `git config {key} <value>` and try again"
                )));
            }
        }
        Ok(())
    }

    pub fn is_dirty(&self, main: &Path) -> Result<bool, GitError> {
        Ok(!self
            .run(main, &["status", "--porcelain"])?
            .trim()
            .is_empty())
    }

    /// Adds `.onerig/` to the exclude file all worktrees share, once.
    pub fn ensure_excluded(&self, main: &Path) -> Result<(), GitError> {
        let listed = self.run(main, &["rev-parse", "--git-path", "info/exclude"])?;
        let path = main.join(listed.trim());
        let current = read_if_present(&path).map_err(|e| file_error(&path, e))?;
        match with_exclude_line(&current) {
            Some(updated) => write_creating_dirs(&path, &updated).map_err(|e| file_error(&path, e)),
            None => Ok(()),
        }
    }

    pub fn create_run_worktree(
        &self,
        main: &Path,
        worktree: &Path,
        branch: &str,
    ) -> Result<(), GitError> {
        let path = worktree.to_string_lossy();
        self.run(main, &["worktree", "add", "-b", branch, &path, "HEAD"])
            .map(|_| ())
    }

    pub fn head(&self, worktree: &Path) -> Result<String, GitError> {
        Ok(self
            .run(worktree, &["rev-parse", "HEAD"])?
            .trim()
            .to_string())
    }

    /// Stages everything, so new files are included, and diffs against `base`.
    pub fn task_diff(&self, worktree: &Path, base: &str) -> Result<TaskDiff, GitError> {
        self.run(worktree, &["add", "-A"])?;
        Ok(TaskDiff {
            stat: self.run(worktree, &["diff", "--cached", "--stat", base])?,
            diff: self.run(worktree, &["diff", "--cached", base])?,
        })
    }

    /// Makes the task one commit on the run branch, folding in any commit a
    /// worker made and putting `HEAD` back on the branch if it was moved.
    pub fn commit_task(
        &self,
        worktree: &Path,
        branch: &str,
        base: &str,
        title: &str,
    ) -> Result<Committed, GitError> {
        let reference = format!("refs/heads/{branch}");
        self.run(worktree, &["symbolic-ref", "HEAD", &reference])?;
        self.run(worktree, &["add", "-A"])?;
        self.run(worktree, &["reset", "--soft", base])?;
        if self.check(worktree, &["diff", "--cached", "--quiet", base])? {
            return Ok(Committed::Nothing);
        }
        self.run(worktree, &["commit", "-m", title])?;
        let hash = self.run(worktree, &["rev-parse", "--short", "HEAD"])?;
        Ok(Committed::Commit(hash.trim().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_home_is_xdg_data_home_when_it_is_set() {
        let home = data_home(Some("/xdg/data"), Some("/home/u"));

        assert_eq!(home, Some(PathBuf::from("/xdg/data")));
    }

    #[test]
    fn data_home_falls_back_to_local_share_under_home() {
        let unset = data_home(None, Some("/home/u"));
        let empty = data_home(Some(""), Some("/home/u"));
        let relative = data_home(Some("data"), Some("/home/u"));

        assert_eq!(unset, Some(PathBuf::from("/home/u/.local/share")));
        assert_eq!(empty, unset);
        assert_eq!(relative, unset);
    }

    #[test]
    fn data_home_is_unknown_without_a_usable_variable() {
        assert_eq!(data_home(None, None), None);
        assert_eq!(data_home(Some(""), Some("")), None);
    }

    #[test]
    fn worktree_path_is_under_the_data_home_by_repo_name_and_run_id() {
        let path = worktree_path(
            Path::new("/data"),
            Path::new("/home/u/code/shop"),
            "20261123-201500-a1f3",
        );

        assert_eq!(
            path,
            PathBuf::from("/data/onerig/worktrees/shop/20261123-201500-a1f3")
        );
    }

    #[test]
    fn worktree_path_of_a_repo_without_a_name_has_no_repo_folder() {
        let path = worktree_path(Path::new("/data"), Path::new("/"), "r1");

        assert_eq!(path, PathBuf::from("/data/onerig/worktrees/r1"));
    }

    #[test]
    fn the_first_worktree_entry_is_taken_from_a_porcelain_listing() {
        let listing = "worktree /home/u/my shop\nHEAD 3f2a\nbranch refs/heads/main\n\n\
                       worktree /data/onerig/worktrees/shop/r1\nHEAD 3f2a\n";

        assert_eq!(
            first_worktree(listing),
            Some(PathBuf::from("/home/u/my shop"))
        );
        assert_eq!(first_worktree(""), None);
    }

    #[test]
    fn every_git_call_names_its_directory_and_cannot_prompt() {
        let command = Git::new().command(Path::new("/repo"), &["status"]);

        let args: Vec<_> = command.get_args().collect();
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(command.get_program(), "git");
        assert_eq!(args, ["-C", "/repo", "status"]);
        assert!(envs.contains(&(OsStr::new("GIT_TERMINAL_PROMPT"), Some(OsStr::new("0")))));
    }

    // A hook or an alias that starts onerig exports these, and they would send
    // every command to that repository instead of the directory named.
    #[test]
    fn inherited_variables_that_name_another_repository_are_removed() {
        let command = Git::new().command(Path::new("/repo"), &["status"]);

        let envs: Vec<_> = command.get_envs().collect();
        for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            assert!(envs.contains(&(OsStr::new(name), None)), "{name}");
        }
    }

    #[test]
    fn a_command_line_reads_like_what_a_user_would_type() {
        let line = command_line(Path::new("/repo"), &["commit", "-m", "Add parser"]);

        assert_eq!(line, "git -C /repo commit -m Add parser");
    }

    #[test]
    fn the_exclude_line_is_appended_on_its_own_line_once() {
        assert_eq!(with_exclude_line(""), Some(".onerig/\n".into()));
        assert_eq!(with_exclude_line("*.log"), Some("*.log\n.onerig/\n".into()));
        assert_eq!(
            with_exclude_line("*.log\n"),
            Some("*.log\n.onerig/\n".into())
        );
        assert_eq!(with_exclude_line("a\n.onerig/\nb\n"), None);
    }

    // Integration tests against real repositories.

    struct Repo {
        _dir: tempfile::TempDir,
        main: PathBuf,
        git: Git,
    }

    impl Repo {
        fn git(&self, dir: &Path, args: &[&str]) -> String {
            self.git.run(dir, args).unwrap()
        }

        fn worktree(&self, name: &str) -> PathBuf {
            let path = self.main.parent().unwrap().join(name);
            self.git
                .create_run_worktree(&self.main, &path, &format!("onerig/{name}"))
                .unwrap();
            path
        }
    }

    /// A repository with one commit and a local identity.
    fn repo() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().canonicalize().unwrap().join("shop");
        std::fs::create_dir(&main).unwrap();
        let repo = Repo {
            _dir: dir,
            main,
            git: Git::isolated(),
        };
        repo.git(&repo.main, &["init", "-q", "-b", "main"]);
        repo.git(&repo.main, &["config", "user.name", "Test"]);
        repo.git(
            &repo.main,
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repo.main.join("README"), "hello\n").unwrap();
        repo.git(&repo.main, &["add", "README"]);
        repo.git(&repo.main, &["commit", "-q", "-m", "first"]);
        repo
    }

    #[test]
    fn the_main_checkout_is_found_from_inside_a_linked_worktree() {
        let repo = repo();
        let worktree = repo.worktree("r1");

        assert_eq!(repo.git.main_checkout(&worktree).unwrap(), repo.main);
        assert_eq!(repo.git.main_checkout(&repo.main).unwrap(), repo.main);
    }

    #[test]
    fn preflight_needs_a_commit_and_an_identity() {
        let empty = tempfile::tempdir().unwrap();
        let git = Git::isolated();
        git.run(empty.path(), &["init", "-q"]).unwrap();
        let repo = repo();

        let no_commit = git.preflight(empty.path());
        repo.git(&repo.main, &["config", "--unset", "user.email"]);
        let no_email = repo.git.preflight(&repo.main);

        assert!(matches!(no_commit, Err(GitError::Preflight(m)) if m.contains("no commits")));
        assert!(matches!(no_email, Err(GitError::Preflight(m)) if m.contains("user.email")));
    }

    #[test]
    fn a_ready_repository_passes_preflight() {
        let repo = repo();

        assert_eq!(repo.git.preflight(&repo.main), Ok(()));
    }

    #[test]
    fn an_untracked_file_makes_the_checkout_dirty() {
        let repo = repo();

        assert!(!repo.git.is_dirty(&repo.main).unwrap());
        std::fs::write(repo.main.join("new"), "x").unwrap();
        assert!(repo.git.is_dirty(&repo.main).unwrap());
    }

    #[test]
    fn the_exclude_line_is_added_once_and_hides_run_files_in_every_worktree() {
        let repo = repo();
        repo.git.ensure_excluded(&repo.main).unwrap();
        repo.git.ensure_excluded(&repo.main).unwrap();
        let worktree = repo.worktree("r1");
        let base = repo.git.head(&worktree).unwrap();
        std::fs::create_dir(worktree.join(".onerig")).unwrap();
        std::fs::write(worktree.join(".onerig/handover.md"), "note").unwrap();

        let diff = repo.git.task_diff(&worktree, &base).unwrap();

        let exclude = std::fs::read_to_string(repo.main.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == ".onerig/").count(), 1);
        assert!(diff.diff.is_empty(), "{}", diff.diff);
    }

    #[test]
    fn the_task_diff_includes_new_files() {
        let repo = repo();
        let worktree = repo.worktree("r1");
        let base = repo.git.head(&worktree).unwrap();
        std::fs::write(worktree.join("added.txt"), "new line\n").unwrap();

        let diff = repo.git.task_diff(&worktree, &base).unwrap();

        assert!(diff.stat.contains("added.txt"));
        assert!(diff.diff.contains("+new line"));
    }

    #[test]
    fn a_task_becomes_one_commit_and_the_main_checkout_is_untouched() {
        let repo = repo();
        let main_head = repo.git.head(&repo.main).unwrap();
        let worktree = repo.worktree("r1");
        let base = repo.git.head(&worktree).unwrap();
        std::fs::write(worktree.join("a.txt"), "a\n").unwrap();

        let committed = repo
            .git
            .commit_task(&worktree, "onerig/r1", &base, "Add a")
            .unwrap();

        assert!(matches!(committed, Committed::Commit(ref hash) if !hash.is_empty()));
        assert_eq!(
            repo.git(&worktree, &["log", "-1", "--format=%s"]).trim(),
            "Add a"
        );
        assert_eq!(
            repo.git(
                &worktree,
                &["rev-list", "--count", &format!("{base}..HEAD")]
            )
            .trim(),
            "1"
        );
        assert_eq!(repo.git.head(&repo.main).unwrap(), main_head);
        assert!(!repo.git.is_dirty(&repo.main).unwrap());
    }

    #[test]
    fn a_commit_the_worker_made_is_folded_into_the_task_commit() {
        let repo = repo();
        let worktree = repo.worktree("r1");
        let base = repo.git.head(&worktree).unwrap();
        std::fs::write(worktree.join("a.txt"), "a\n").unwrap();
        repo.git(&worktree, &["add", "a.txt"]);
        repo.git(&worktree, &["commit", "-q", "-m", "worker"]);
        std::fs::write(worktree.join("b.txt"), "b\n").unwrap();

        repo.git
            .commit_task(&worktree, "onerig/r1", &base, "Task")
            .unwrap();

        assert_eq!(
            repo.git(
                &worktree,
                &["rev-list", "--count", &format!("{base}..HEAD")]
            )
            .trim(),
            "1"
        );
        let files = repo.git(&worktree, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(files.lines().collect::<Vec<_>>(), ["a.txt", "b.txt"]);
    }

    #[test]
    fn a_task_that_changed_nothing_makes_no_commit() {
        let repo = repo();
        let worktree = repo.worktree("r1");
        let base = repo.git.head(&worktree).unwrap();

        let committed = repo
            .git
            .commit_task(&worktree, "onerig/r1", &base, "Nothing")
            .unwrap();

        assert_eq!(committed, Committed::Nothing);
        assert_eq!(repo.git.head(&worktree).unwrap(), base);
    }

    #[test]
    fn head_is_put_back_on_the_run_branch_when_a_worker_moved_it() {
        let repo = repo();
        let worktree = repo.worktree("r1");
        let base = repo.git.head(&worktree).unwrap();
        repo.git(&worktree, &["checkout", "-q", "-b", "elsewhere"]);
        std::fs::write(worktree.join("a.txt"), "a\n").unwrap();
        repo.git(&worktree, &["add", "a.txt"]);
        repo.git(&worktree, &["commit", "-q", "-m", "worker"]);

        repo.git
            .commit_task(&worktree, "onerig/r1", &base, "Task")
            .unwrap();

        assert_eq!(
            repo.git(&worktree, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/onerig/r1"
        );
        assert_eq!(
            repo.git(&worktree, &["log", "-1", "--format=%s", "onerig/r1"])
                .trim(),
            "Task"
        );
    }

    #[test]
    fn a_failed_git_call_names_the_command_and_gives_gits_message() {
        let repo = repo();

        let error = repo.git.head(&repo.main.join("missing")).unwrap_err();

        assert!(matches!(&error, GitError::Failed { command, stderr }
            if command.ends_with("rev-parse HEAD") && !stderr.is_empty()));
    }
}
