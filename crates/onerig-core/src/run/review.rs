//! Reviewing: asking the reviewer, applying the verdict and committing the
//! task.

use super::*;

impl Machine<'_> {
    pub(super) fn review(&mut self) -> Result<(), RunError> {
        if self.state.approved {
            return self.commit();
        }
        let Some(prompt) = self.review_prompt()? else {
            return Ok(());
        };
        let lane = self.config.roles.reviewer.clone();
        match self.ask(
            &lane,
            Role::Reviewer,
            &prompt,
            VERDICT_SCHEMA,
            parse_verdict,
        )? {
            Asked::Answer(verdict) => self.apply_verdict(verdict),
            Asked::Pause(reason, message, until) => {
                self.pause(reason, Phase::Reviewing, message, until)
            }
            Asked::Failed(problem) => self.pause(
                PauseReason::JobFailed,
                Phase::Reviewing,
                format!("the reviewer failed twice: {problem}"),
                None,
            ),
            Asked::Stopped => Ok(()),
        }
    }

    /// `None` when the run has paused or failed instead.
    fn review_prompt(&mut self) -> Result<Option<String>, RunError> {
        let (Some(task), Some(base)) = (self.task().cloned(), self.state.task_base.clone()) else {
            self.fail("the run has no current task to review".to_string())?;
            return Ok(None);
        };
        match self.git.task_diff(&self.state.worktree, &base) {
            Ok(diff) => Ok(Some(prompt::reviewer_prompt(
                &self.state.goal,
                &task,
                &diff.stat,
                &diff.diff,
            ))),
            Err(error) => {
                self.pause(
                    PauseReason::GitError,
                    Phase::Reviewing,
                    error.to_string(),
                    None,
                )?;
                Ok(None)
            }
        }
    }

    fn apply_verdict(&mut self, verdict: Verdict) -> Result<(), RunError> {
        self.note(LogEntry::State(StateEvent::Verdict {
            verdict: verdict.verdict,
            summary: verdict.summary.clone(),
            issues: verdict.issues.clone(),
        }));
        match verdict.verdict {
            VerdictKind::Approve => {
                self.state.approved = true;
                self.save()?;
                self.commit()
            }
            VerdictKind::Revise => {
                self.state.issues = verdict.issues;
                self.state.last_end = Some(LastEnd::Revise);
                self.set_phase(Phase::Executing);
                self.save()
            }
        }
    }

    fn commit(&mut self) -> Result<(), RunError> {
        let (Some(task), Some(base)) = (self.task().cloned(), self.state.task_base.clone()) else {
            return self.fail("the run has no current task to commit".to_string());
        };
        let committed =
            self.git
                .commit_task(&self.state.worktree, &self.state.branch, &base, &task.title);
        match committed {
            Err(error) => self.pause(
                PauseReason::GitError,
                Phase::Reviewing,
                error.to_string(),
                None,
            ),
            Ok(committed) => {
                if let Committed::Commit(hash) = committed {
                    self.note(LogEntry::State(StateEvent::Commit {
                        hash,
                        title: task.title,
                    }));
                }
                self.state.approved = false;
                remove_if_present(&handover_note(&self.state.worktree))?;
                self.next_task()
            }
        }
    }

    fn next_task(&mut self) -> Result<(), RunError> {
        let index = self.state.task_index.unwrap_or(0);
        let total = self.plan.as_ref().map_or(0, |plan| plan.tasks.len());
        if index < total {
            return self.start_task(index + 1);
        }
        self.set_phase(Phase::Done);
        let branch = self.state.branch.clone();
        self.note(LogEntry::State(StateEvent::Done { branch }));
        self.save()
    }
}
