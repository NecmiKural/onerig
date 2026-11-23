//! Executing: choosing the lane, running a worker attempt, writing the
//! hand-over and handling a failed worker job.

use super::*;

impl Machine<'_> {
    pub(super) fn execute(&mut self) -> Result<(), RunError> {
        if self.state.task_base.is_none() {
            match self.git.head(&self.state.worktree) {
                Ok(head) => self.state.task_base = Some(head),
                Err(error) => {
                    return self.pause(
                        PauseReason::GitError,
                        Phase::Executing,
                        error.to_string(),
                        None,
                    );
                }
            }
        }
        if policy::budget_exhausted(self.state.attempts_used, self.config.run.max_attempts) {
            let message = format!(
                "task {} used {} attempts without an approval",
                self.state.task_index.unwrap_or(0),
                self.state.attempts_used
            );
            return self.pause(
                PauseReason::AttemptsExhausted,
                Phase::Executing,
                message,
                None,
            );
        }
        let Selected {
            selection,
            new_marks,
        } = self.select();
        for (lane, mark) in new_marks {
            self.mark_exhausted(&lane, mark.exhausted_until);
        }
        match selection {
            Selection::NoLane { message, until } => {
                self.pause(PauseReason::NoLane, Phase::Executing, message, until)
            }
            Selection::Lane { name, manual } => self.attempt(name, manual),
        }
    }

    fn select(&mut self) -> Selected {
        let now = self.now();
        let infos: Vec<(String, bool, bool, Option<String>, LaneMark, _)> = self
            .config
            .roles
            .worker
            .iter()
            .map(|name| {
                let lane = &self.config.lanes[name];
                let automatic = !lane.is_off() && !lane.is_manual();
                let quota = if automatic {
                    self.runner.quota(name)
                } else {
                    None
                };
                let mark = self.state.lanes.get(name).copied().unwrap_or_default();
                let probe = self.probes.failed.get(name).cloned();
                (
                    name.clone(),
                    lane.is_off(),
                    lane.is_manual(),
                    probe,
                    mark,
                    quota,
                )
            })
            .collect();
        for (lane, report) in infos.iter().filter_map(|i| Some((&i.0, i.5?))) {
            self.log.record(
                self.state.task_index,
                None,
                None,
                LogEntry::Policy(PolicyEvent::Quota {
                    lane: lane.clone(),
                    remaining_fraction: report.remaining_fraction,
                    reset_at: report.reset_at,
                }),
            );
        }
        let views: Vec<LaneView<'_>> = infos
            .iter()
            .map(|(name, off, manual, probe, mark, quota)| LaneView {
                name,
                off: *off,
                manual: *manual,
                probe_error: probe.as_deref(),
                mark: *mark,
                quota: *quota,
            })
            .collect();
        policy::select(
            &views,
            self.state.lane.as_deref(),
            &self.state.skipped,
            self.config.policy.min_remaining,
            now,
        )
    }

    fn mark_exhausted(&mut self, lane: &str, until: Option<u64>) {
        let mark = LaneMark {
            exhausted: true,
            exhausted_until: until,
        };
        self.state.lanes.insert(lane.to_string(), mark);
        self.note(LogEntry::Policy(PolicyEvent::LaneExhausted {
            lane: lane.to_string(),
            until,
        }));
    }

    fn attempt(&mut self, name: String, manual: bool) -> Result<(), RunError> {
        if self.state.lane.as_deref() != Some(name.as_str()) {
            self.state.unknown_streak = 0;
        }
        self.state.lane = Some(name.clone());
        let attempt = self.state.attempt.unwrap_or(0) + 1;
        self.state.attempt = Some(attempt);
        self.state.attempts_used += 1;
        self.note(LogEntry::Policy(PolicyEvent::LaneSelected {
            lane: name.clone(),
            manual,
        }));
        let prompt = match self.worker_prompt(attempt, manual) {
            Ok(prompt) => prompt,
            Err(error) => {
                return self.pause(
                    PauseReason::GitError,
                    Phase::Executing,
                    error.to_string(),
                    None,
                );
            }
        };
        self.write_handover(attempt, &prompt)?;
        if manual {
            return self.pause_for_human(&name);
        }
        let Some((outcome, text)) = self.job(&name, Role::Worker, prompt, None)? else {
            return Ok(());
        };
        self.after_worker(&name, outcome, &text)
    }

    pub(super) fn worker_prompt(&self, attempt: u32, tell_user: bool) -> Result<String, GitError> {
        let state = &self.state;
        let (Some(task), Some(plan), Some(base), Some(index)) =
            (self.task(), &self.plan, &state.task_base, state.task_index)
        else {
            return Err(GitError::Preflight("the run has no current task".into()));
        };
        let diff = match state.last_end {
            Some(_) => Some(self.git.task_diff(&state.worktree, base)?),
            None => None,
        };
        let previous = state
            .last_end
            .zip(diff.as_ref())
            .map(|(reason, diff)| Previous {
                reason,
                diff_stat: &diff.stat,
                diff: &diff.diff,
                last_output: &state.last_output_tail,
            });
        Ok(prompt::worker_prompt(&WorkerPrompt {
            run_id: &state.run_id,
            goal: &state.goal,
            task,
            task_number: index,
            task_total: plan.tasks.len(),
            attempt,
            worktree: &state.worktree,
            branch: &state.branch,
            previous,
            issues: &state.issues,
            tell_user_to_resume: tell_user,
        }))
    }

    /// The archive copy in the run directory and the note in the worktree.
    pub(super) fn write_handover(&mut self, attempt: u32, prompt: &str) -> Result<(), RunError> {
        let archive = self
            .log
            .dir
            .handover_path(self.state.task_index.unwrap_or(0), attempt);
        let note = handover_note(&self.state.worktree);
        let write = |path: &Path| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, prompt)
        };
        for path in [&archive, &note] {
            write(path).map_err(|e| StoreError::Io {
                path: path.clone(),
                message: e.to_string(),
            })?;
        }
        self.note(LogEntry::Policy(PolicyEvent::Handover { path: archive }));
        Ok(())
    }

    fn pause_for_human(&mut self, lane: &str) -> Result<(), RunError> {
        let message = format!(
            "cd {} && {}\n\
             First message: Read .onerig/handover.md and follow its Instruction section.\n\
             When you are done (do not commit): onerig resume {}",
            self.state.worktree.display(),
            self.config.lanes[lane].bin,
            self.state.run_id
        );
        self.pause(PauseReason::Handover, Phase::Reviewing, message, None)
    }

    fn after_worker(&mut self, lane: &str, outcome: Outcome, text: &str) -> Result<(), RunError> {
        match outcome {
            Outcome::Done {
                text: final_text, ..
            } => {
                let shown = if text.is_empty() {
                    final_text.as_str()
                } else {
                    text
                };
                self.state.last_output_tail = prompt::output_tail(shown).to_string();
                self.state.unknown_streak = 0;
                self.set_phase(Phase::Reviewing);
                self.save()
            }
            Outcome::Failed {
                kind,
                message,
                reset_at,
            } => {
                let shown = if text.is_empty() {
                    message.as_str()
                } else {
                    text
                };
                self.state.last_output_tail = prompt::output_tail(shown).to_string();
                self.state.last_end = Some(policy::last_end(kind));
                self.after_failure(lane, kind, reset_at, message)
            }
        }
    }

    fn after_failure(
        &mut self,
        lane: &str,
        kind: FailureKind,
        reset_at: Option<u64>,
        message: String,
    ) -> Result<(), RunError> {
        let streak = self.state.unknown_streak;
        let action = policy::after_failure(kind, reset_at, streak, self.now());
        // Only a retry on the same lane continues the run of Unknown failures.
        self.state.unknown_streak = if action == AfterFailure::RetrySameLane {
            streak + 1
        } else {
            0
        };
        match action {
            AfterFailure::MarkExhausted { until } => self.mark_exhausted(lane, until),
            AfterFailure::PauseAuth => {
                return self.pause(PauseReason::Auth, Phase::Executing, message, None);
            }
            AfterFailure::SkipLane => {
                self.state.skipped.push(lane.to_string());
                self.note(LogEntry::Policy(PolicyEvent::LaneSkipped {
                    lane: lane.to_string(),
                }));
            }
            AfterFailure::RetrySameLane => {}
        }
        self.save()
    }
}
