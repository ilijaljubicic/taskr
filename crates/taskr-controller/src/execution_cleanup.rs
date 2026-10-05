use super::*;

static CLEANUP_LOCKS: std::sync::LazyLock<
    Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn cleanup_lock(execution_id: &str) -> Result<Arc<tokio::sync::Mutex<()>>, String> {
    let mut locks = CLEANUP_LOCKS
        .lock()
        .map_err(|_| "worker cleanup registry lock poisoned")?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(execution_id).and_then(std::sync::Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(execution_id.to_owned(), Arc::downgrade(&lock));
    Ok(lock)
}

pub(super) struct ClosedWorker {
    pub execution: TaskExecution,
    pub pane_id: Option<String>,
    pub note: String,
}

/// Save conversation/report references, exit the worker identified by this
/// durable execution, and close its verified pane. Used by both
/// explicit stop and final task-state cleanup, including retries after restart.
pub(super) async fn close_worker(
    ctx: &LaunchContext,
    task_id: &TaskId,
    execution: &TaskExecution,
) -> Result<ClosedWorker, String> {
    endpoint_migration::require_resolved_endpoint(&execution.endpoint_id)?;
    if execution.phase == ExecutionPhase::Stopped && execution.pane_closed {
        return Ok(ClosedWorker {
            execution: execution.clone(),
            pane_id: execution.pane_id.clone(),
            note: "worker and pane already closed".into(),
        });
    }
    // A status transition and retry sweep can select the same worker. Serialize
    // their interactive exit sequence, then re-read the current binding.
    let lock = cleanup_lock(&execution.execution_id)?;
    let _guard = lock.lock().await;
    let current = ctx.orchestration.snapshot()?;
    if current
        .tasks
        .get(task_id)
        .and_then(|task| task.execution.as_ref())
        .is_none_or(|current| current.execution_id != execution.execution_id)
    {
        return Err("execution changed before worker cleanup".into());
    }
    let execution = current.tasks[task_id].execution.as_ref().unwrap();
    if execution.phase == ExecutionPhase::Stopped && execution.pane_closed {
        return Ok(ClosedWorker {
            execution: execution.clone(),
            pane_id: execution.pane_id.clone(),
            note: "worker and pane already closed".into(),
        });
    }
    let mut saved_execution = execution.clone();
    if saved_execution.report.is_none() {
        let task = &current.tasks[task_id];
        saved_execution.report = Some(taskr_core::orchestration::ExecutionReport {
            task_status: task.status,
            outcome: task.outcome.clone(),
            evidence: task.evidence.clone(),
        });
        // Preserve the attempt's report even if the endpoint is unavailable or
        // ownership checks prevent exit. A subsequent attempt cannot replace it.
        saved_execution = ctx
            .orchestration
            .record_execution_if_current(task_id.clone(), saved_execution)?;
    }
    let execution = &saved_execution;
    if taskr_core::coordination::unresolved_allocation(execution) {
        return Err(
            "allocation outcome is unresolved; cannot prove ownership or closure without a receipt"
                .into(),
        );
    }
    let target = ctx
        .herdr
        .target_for_endpoint(&execution.endpoint_id)
        .fenced(execution.runtime_generation.clone());
    let agent_ref = HerdrMcpServer::execution_agent_ref(execution);
    let mut pane_id = execution.pane_id.clone();
    let mut live_agent = None;
    let mut observed_execution = execution.clone();
    if let Some(agent_ref) = agent_ref {
        match ctx.herdr.agent_get(&target, &agent_ref).await {
            Ok(info) => {
                if !HerdrMcpServer::execution_owns_agent(execution, &info) {
                    return Err(format!("refusing to close execution '{}': current occupant is not the recorded worker", execution.execution_id));
                }
                pane_id = pane_id.or(info.pane_id.clone());
                if observed_execution.agent_session.is_none() {
                    observed_execution.agent_session = info.agent_session.clone();
                }
                if observed_execution.terminal_id.is_none() {
                    observed_execution.terminal_id = info
                        .raw
                        .get("terminal_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                if info.kind.is_some() {
                    if execution.phase == ExecutionPhase::Stopped {
                        return Err("stopped pane has been reopened; explicit adoption is required before cleanup".into());
                    }
                    live_agent = Some(info);
                } else if let Some(pane_id) = pane_id.as_deref() {
                    if !foreground_is_shell(&ctx.herdr, &target, pane_id).await? {
                        return Err("worker foreground is not a verified agent or shell".into());
                    }
                }
            }
            Err(error) if error.category == taskr_herdr::RuntimeErrorCategory::MissingTarget => {
                if let Some(pane_id) = pane_id.as_deref() {
                    match ctx.herdr.pane_get(&target, pane_id).await {
                        Ok(pane) => {
                            let occupant =
                                taskr_herdr::AgentInfo::parse(pane.get("pane").unwrap_or(&pane));
                            if occupant.kind.is_some() {
                                return Err(format!("refusing to close execution '{}': pane '{pane_id}' has another occupant", execution.execution_id));
                            }
                            if !foreground_is_shell(&ctx.herdr, &target, pane_id).await? {
                                return Err("worker pane is not a verified shell".into());
                            }
                        }
                        Err(error)
                            if error.category
                                == taskr_herdr::RuntimeErrorCategory::MissingTarget => {}
                        Err(error) => {
                            return Err(format!("worker pane could not be observed: {error}"))
                        }
                    }
                }
            }
            Err(error) => return Err(format!("worker could not be observed: {error}")),
        }
    } else if pane_id.is_some() {
        return Err(
            "worker binding has no agent identity; cannot establish cleanup ownership".into(),
        );
    }
    // Save the report and identities before `/exit`, while Herdr can still observe the agent.
    // Once it exits, the pane no longer exposes the native conversation ID.
    if observed_execution != *execution {
        ctx.orchestration
            .record_execution_if_current(task_id.clone(), observed_execution.clone())?;
    }
    let execution = &observed_execution;
    if let Some(info) = live_agent {
        if execution.phase == ExecutionPhase::Live && execution.agent_session.is_none() {
            return Err(
                "native conversation ID has not been observed; worker kept so it remains resumable"
                    .into(),
            );
        }
        let kind = info.kind.as_deref().unwrap_or_default();
        if !matches!(kind, "codex" | "claude" | "opencode" | "kimi") {
            return Err(format!(
                "graceful exit is not configured for agent kind '{kind}'"
            ));
        }
        let agent_ref = HerdrMcpServer::execution_agent_ref(execution)
            .ok_or_else(|| "worker has no agent identity".to_owned())?;
        // Interrupt a running turn or dismiss a dialog first. Never send shell
        // `exit`: the interactive agent receives its native slash command.
        let ready = if matches!(info.status.as_deref(), Some("idle" | "done")) {
            info.clone()
        } else {
            ctx.herdr
                .agent_send_keys(&target, &agent_ref, &["esc".into()])
                .await
                .map_err(|error| format!("could not interrupt worker: {error}"))?;
            ctx.herdr
                .agent_wait(
                    &target,
                    &agent_ref,
                    &coding_ready_states(),
                    Duration::from_secs(3),
                )
                .await
                .map_err(|error| format!("worker is not ready for graceful exit: {error}"))?
        };
        if !HerdrMcpServer::execution_owns_agent(execution, &ready) {
            return Err("worker occupant changed before graceful exit".into());
        }
        ctx.herdr
            .agent_send_keys(&target, &agent_ref, &["ctrl+u".into()])
            .await
            .map_err(|error| format!("could not clear worker input: {error}"))?;
        ctx.herdr
            .agent_prompt(
                &target,
                &taskr_herdr::PromptRequest {
                    target: agent_ref,
                    text: "/exit".into(),
                    wait: false,
                    until: Vec::new(),
                    timeout: Some(Duration::from_secs(3)),
                },
            )
            .await
            .map_err(|error| format!("could not submit worker exit: {error}"))?;
        let pane = pane_id
            .as_deref()
            .ok_or_else(|| "worker has no pane binding".to_owned())?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match ctx.herdr.pane_get(&target, pane).await {
                Ok(value) => {
                    let occupant =
                        taskr_herdr::AgentInfo::parse(value.get("pane").unwrap_or(&value));
                    if foreground_is_shell(&ctx.herdr, &target, pane).await? {
                        break;
                    }
                    if occupant.kind.is_some()
                        && (occupant.kind.as_deref() != execution.agent_kind.as_deref()
                            || (execution.agent_session.is_some()
                                && occupant.agent_session.is_some()
                                && occupant.agent_session != execution.agent_session))
                    {
                        return Err("worker occupant changed while exiting".into());
                    }
                }
                Err(error)
                    if error.category == taskr_herdr::RuntimeErrorCategory::MissingTarget =>
                {
                    break
                }
                Err(error) => return Err(format!("could not verify worker exit: {error}")),
            }
            if Instant::now() >= deadline {
                return Err("worker exit is not yet confirmed; pane retained".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if let Some(pane) = pane_id.as_deref() {
        close_owned_pane(ctx, execution, &target, pane).await?;
    }
    let note = "worker exited; pane closed; native conversation reference preserved".to_owned();
    let mut updated = execution.clone();
    updated.phase = ExecutionPhase::Stopped;
    updated.pane_closed = true;
    updated.pane_id = pane_id.clone();
    updated.last_seen_ms = now_ms();
    let execution = ctx
        .orchestration
        .record_execution_if_current(task_id.clone(), updated)?;
    ctx.herdr
        .endpoint(
            target,
            taskr_herdr::EndpointAction::Release {
                lease_id: execution.execution_id.clone(),
            },
        )
        .await
        .map_err(|error| {
            format!("worker closed but endpoint lease release needs retry: {error}")
        })?;
    Ok(ClosedWorker {
        execution,
        pane_id,
        note,
    })
}

async fn close_owned_pane(
    ctx: &LaunchContext,
    execution: &TaskExecution,
    target: &EndpointTarget,
    pane_id: &str,
) -> Result<(), String> {
    let state = ctx.orchestration.snapshot()?;
    let owner = state.tasks.values().find(|task| {
        task.execution
            .as_ref()
            .is_some_and(|current| current.execution_id == execution.execution_id)
    });
    let lock = owner.map(|task| plan_layout::lock_for(&task.plan_id, &execution.endpoint_id));
    let _guard = match &lock {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    let value = match ctx.herdr.pane_get(target, pane_id).await {
        Ok(value) => value,
        Err(error) if error.category == taskr_herdr::RuntimeErrorCategory::MissingTarget => {
            return Ok(())
        }
        Err(error) => return Err(format!("could not verify pane before closing: {error}")),
    };
    let pane = value.get("pane").unwrap_or(&value);
    if execution.terminal_id.is_none()
        || pane.get("terminal_id").and_then(Value::as_str) != execution.terminal_id.as_deref()
        || pane.get("pane_id").and_then(Value::as_str) != Some(pane_id)
        || pane.get("tab_id").and_then(Value::as_str) != execution.tab_id.as_deref()
        || pane.get("workspace_id").and_then(Value::as_str) != execution.workspace_id.as_deref()
    {
        return Err(
            "pane ownership changed or immutable terminal identity is missing; pane kept".into(),
        );
    }
    if !foreground_is_shell(&ctx.herdr, target, pane_id).await? {
        return Err("pane foreground is not the verified shell; pane kept".into());
    }
    ctx.herdr
        .pane_close(target, pane_id)
        .await
        .map_err(|error| format!("could not close worker pane: {error}"))
}

async fn foreground_is_shell(
    herdr: &HerdrClient,
    target: &EndpointTarget,
    pane_id: &str,
) -> Result<bool, String> {
    herdr
        .process_info(target, pane_id)
        .await
        .map(|process| process.is_shell())
        .map_err(|error| format!("could not observe foreground for pane '{pane_id}': {error}"))
}

pub(super) async fn close_finished_worker(
    ctx: &LaunchContext,
    task_id: &TaskId,
) -> Result<Task, String> {
    let state = ctx.orchestration.snapshot()?;
    let task = state
        .tasks
        .get(task_id)
        .ok_or_else(|| "task not found".to_owned())?;
    if task_status_allows_runtime_cleanup(task.status) && audit_holds(&state, task_id).is_empty() {
        if let Some(execution) = task
            .execution
            .as_ref()
            .filter(|execution| !execution.inspection)
        {
            // The allocation command still owns its continuation. Cancel the
            // task now, then consume the receipt before touching its resources.
            let in_flight = startup_registry_snapshot()
                .iter()
                .any(|job| job.execution_id == execution.execution_id && job.state == "starting");
            if !(taskr_core::coordination::unresolved_allocation(execution) && in_flight) {
                close_worker(ctx, task_id, execution).await?;
            }
        }
    }
    ctx.orchestration
        .snapshot()?
        .tasks
        .get(task_id)
        .cloned()
        .ok_or_else(|| "task not found".into())
}

pub(super) fn audit_holds(state: &OrchestrationState, task_id: &TaskId) -> Vec<String> {
    taskr_core::coordination::audit_holds(state, task_id)
}

pub(super) async fn cleanup_after_task_update(
    ctx: &LaunchContext,
    task_id: &TaskId,
) -> Result<Task, String> {
    let state = ctx.orchestration.snapshot()?;
    let mut targets = vec![task_id.clone()];
    targets.extend(
        state
            .task_edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::Audits && &edge.from == task_id)
            .map(|edge| edge.to.clone()),
    );
    targets.sort_by(|left, right| left.0.cmp(&right.0));
    targets.dedup();
    let mut errors = Vec::new();
    for target in targets {
        if let Err(error) = close_finished_worker(ctx, &target).await {
            errors.push(format!("task '{}': {error}", target.0));
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    ctx.orchestration
        .snapshot()?
        .tasks
        .get(task_id)
        .cloned()
        .ok_or_else(|| "task not found".into())
}

/// Final task state is the durable cleanup intent. A failed observation/close
/// leaves it intact, so a later sweep (including the first after restart) retries.
pub(super) async fn sweep_finished_workers(ctx: &LaunchContext) -> Vec<String> {
    let state = match ctx.orchestration.snapshot() {
        Ok(state) => state,
        Err(error) => return vec![error],
    };
    let mut jobs = tokio::task::JoinSet::new();
    let mut errors = Vec::new();
    for task in state.tasks.values().filter(|task| {
        taskr_core::coordination::cleanup_decision(&state, task)
            == taskr_core::coordination::CleanupDecision::Close
    }) {
        let Some(execution) = task.execution.as_ref() else {
            continue;
        };
        if execution.inspection
            || (execution.phase == ExecutionPhase::Stopped && execution.pane_closed)
        {
            continue;
        }
        let ctx = ctx.clone();
        let task_id = task.id.clone();
        jobs.spawn(async move {
            close_finished_worker(&ctx, &task_id)
                .await
                .map(|_| ())
                .map_err(|error| format!("task '{}': {error}", task_id.0))
        });
        // Bound observations without blocking MCP inspection behind cleanup.
        if jobs.len() >= 4 {
            if let Some(result) = jobs.join_next().await {
                match result {
                    Ok(Err(error)) => errors.push(error),
                    Err(error) => errors.push(error.to_string()),
                    _ => {}
                }
            }
        }
    }
    while let Some(result) = jobs.join_next().await {
        match result {
            Ok(Err(error)) => errors.push(error),
            Err(error) => errors.push(error.to_string()),
            _ => {}
        }
    }
    errors
}

/// Select/remove only old, stopped, positively identified shell layouts. A
/// retained shell is still a live pane in Herdr; remove its key from the prune
/// observation only after a safe dry-run selection or successful pane close.
pub(super) async fn prepare_retained_layout_prune(
    herdr: &HerdrClient,
    state: &OrchestrationState,
    dry_run: bool,
    include_executions: bool,
    older_than_days: Option<u64>,
    live: &mut HashSet<String>,
    warnings: &mut Vec<String>,
) {
    if !include_executions {
        return;
    }
    let cutoff = match older_than_days {
        Some(days) => match days
            .checked_mul(86_400_000)
            .and_then(|age| now_ms().checked_sub(age))
        {
            Some(cutoff) => Some(cutoff),
            None => {
                warnings.push("prune age exceeds the supported timestamp range".into());
                return;
            }
        },
        None => None,
    };
    let executions = state
        .tasks
        .values()
        .filter_map(|task| task.execution.as_ref())
        .chain(
            state
                .retained_executions
                .values()
                .map(|retained| &retained.execution),
        );
    for execution in executions {
        if execution.pane_closed
            || execution.phase != ExecutionPhase::Stopped
            || cutoff.is_some_and(|cutoff| execution.last_seen_ms > cutoff)
            || !live.contains(&execution.runtime_key())
        {
            continue;
        }
        if state
            .tasks
            .values()
            .filter_map(|task| task.execution.as_ref())
            .any(|current| {
                current.execution_id != execution.execution_id
                    && current.runtime_key() == execution.runtime_key()
            })
        {
            continue; // A newer execution now owns this pane, even during startup.
        }
        if execution.agent_name.as_ref().is_some_and(|name| {
            live.contains(&taskr_core::orchestration::runtime_key_for(
                &execution.endpoint_id,
                execution.runtime_generation.as_deref(),
                name,
            ))
        }) {
            continue; // A recognized agent has resumed or replaced this worker.
        }
        let Some(pane_id) = execution.pane_id.as_deref() else {
            continue;
        };
        let target = herdr
            .target_for_endpoint(&execution.endpoint_id)
            .fenced(execution.runtime_generation.clone());
        let value = match herdr.pane_get(&target, pane_id).await {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!(
                    "endpoint '{}' retained pane '{pane_id}' could not be verified: {error}",
                    execution.endpoint_id
                ));
                continue;
            }
        };
        let pane = value.get("pane").unwrap_or(&value);
        let occupant = taskr_herdr::AgentInfo::parse(pane);
        let owned_shell = occupant.kind.is_none()
            && occupant.pane_id.as_deref() == Some(pane_id)
            && execution.workspace_id.is_some()
            && occupant.workspace_id == execution.workspace_id
            && execution.tab_id.is_some()
            && occupant.tab_id == execution.tab_id
            && execution.terminal_id.is_some()
            && pane.get("terminal_id").and_then(Value::as_str) == execution.terminal_id.as_deref();
        if !owned_shell {
            warnings.push(format!("endpoint '{}' retained pane '{pane_id}' was reused or is not a verified owned shell; kept", execution.endpoint_id));
            continue;
        }
        // A non-agent foreground command may still be classified `shelling`.
        // Require the shell itself to be the only foreground process before
        // pruning; preserve user commands started in a retained terminal.
        let shell_foreground = match foreground_is_shell(herdr, &target, pane_id).await {
            Ok(shell) => shell,
            Err(error) => {
                warnings.push(format!("endpoint '{}' retained pane '{pane_id}' foreground could not be observed: {error}", execution.endpoint_id));
                continue;
            }
        };
        if !shell_foreground {
            warnings.push(format!(
                "endpoint '{}' retained pane '{pane_id}' has another foreground process; kept",
                execution.endpoint_id
            ));
            continue;
        }
        if !dry_run {
            if let Err(error) = herdr.pane_close(&target, pane_id).await {
                if error.category != taskr_herdr::RuntimeErrorCategory::MissingTarget {
                    warnings.push(format!(
                        "endpoint '{}' retained pane '{pane_id}' could not be pruned: {error}",
                        execution.endpoint_id
                    ));
                    continue;
                }
            }
        }
        live.remove(&execution.runtime_key());
    }
}

/// Native timer / DO alarm responsibility, independent of cleanup selection.
pub(super) async fn renew_endpoint_leases(ctx: &LaunchContext) -> Vec<String> {
    let state = match ctx.orchestration.snapshot() {
        Ok(state) => state,
        Err(error) => return vec![error],
    };
    let mut errors = Vec::new();
    for execution in state
        .tasks
        .values()
        .filter_map(|task| task.execution.as_ref())
        .chain(state.retained_executions.values().map(|row| &row.execution))
    {
        let target = ctx
            .herdr
            .target_for_endpoint(&execution.endpoint_id)
            .fenced(execution.runtime_generation.clone());
        let action = if taskr_core::coordination::needs_endpoint_lease(execution) {
            taskr_herdr::EndpointAction::Renew {
                lease_id: execution.execution_id.clone(),
            }
        } else {
            taskr_herdr::EndpointAction::Release {
                lease_id: execution.execution_id.clone(),
            }
        };
        if let Err(error) = ctx.herdr.endpoint(target.clone(), action).await {
            if error.category == taskr_herdr::RuntimeErrorCategory::GenerationMismatch {
                if let Some(observed) = error.observed_generation.as_deref() {
                    if ctx
                        .orchestration
                        .namespace_lost(&execution.execution_id, observed)
                        .is_ok()
                    {
                        let _ = ctx
                            .herdr
                            .endpoint(
                                target,
                                taskr_herdr::EndpointAction::Release {
                                    lease_id: execution.execution_id.clone(),
                                },
                            )
                            .await;
                        continue;
                    }
                }
            }
            errors.push(format!(
                "execution '{}' endpoint lease action failed: {error}",
                execution.execution_id
            ));
        }
    }
    errors
}
