//! Reopen a saved native conversation in a fresh terminal, without task mutation.
use super::*;

pub(super) use taskr_core::coordination::resume_args;

pub(super) async fn resume(
    ctx: &LaunchContext,
    execution_id: &str,
) -> Result<LaunchOutcome, String> {
    let state = ctx.orchestration.snapshot()?;
    let (task_id, source) = if let Some(task) = state.tasks.values().find(|task| {
        task.execution
            .as_ref()
            .is_some_and(|execution| execution.execution_id == execution_id)
    }) {
        (task.id.clone(), task.execution.as_ref().unwrap().clone())
    } else if let Some(row) = state.retained_executions.get(execution_id) {
        (row.task_id.clone(), row.execution.clone())
    } else {
        return Err(format!("execution '{execution_id}' not found"));
    };
    let task = state
        .tasks
        .get(&task_id)
        .ok_or("execution's task has been removed")?;
    taskr_core::coordination::resume_allowed(&source, task.execution.as_ref())?;
    endpoint_migration::require_resolved_endpoint(&source.endpoint_id)?;
    let kind = source
        .agent_kind
        .as_deref()
        .ok_or("saved execution has no native agent kind")?;
    let session = source
        .agent_session
        .as_deref()
        .ok_or("saved execution has no native session ID; cannot resume")?;
    let choice =
        ctx.resolve_task_profile(&state, task, &source.endpoint_id, &source.launch_profile_id)?;
    let profile = &choice.profile;
    if profile.agent_kind != kind {
        return Err("saved launch profile now selects a different agent kind".into());
    }
    if let Some(deployment) = choice.deployment {
        let variable =
            taskr_herdr::home_env_var_for_kind(kind).ok_or("unknown deployed agent kind")?;
        if source.launch_env.get(variable) != Some(&deployment.home) {
            return Err("saved conversation home does not match its pinned deployment; refusing to redirect resume".into());
        }
    }
    ctx.launch_profiles
        .verify(&ctx.herdr, &source.endpoint_id, &source.launch_profile_id)
        .await?;
    let args = resume_args(kind, &source.launch_args, session)?;
    let mut execution = source.clone();
    execution.execution_id = new_execution_id();
    execution.agent_name = Some(generate_agent_name(
        &task.slug,
        kind,
        short_attempt_suffix(),
    ));
    execution.inspection = true;
    execution.resumed_from = Some(source.execution_id.clone());
    execution.report = None;
    execution.workspace_id = None;
    execution.tab_id = None;
    execution.pane_id = None;
    execution.terminal_id = None;
    execution.runtime_generation = None;
    execution.pane_closed = false;
    execution.phase = ExecutionPhase::Pending;
    execution.recovery = ExecutionRecovery::NeedsReconciliation;
    execution.launch_args = args.clone();
    let resolved = ResolvedLaunch {
        endpoint_id: source.endpoint_id.clone(),
        launch_profile_id: source.launch_profile_id.clone(),
        agent_kind: kind.to_owned(),
        args,
        env: source.launch_env.clone(),
        bypass_permissions: source.bypass_permissions,
        workspace_path: source.workspace_path.clone(),
        skills: source.skills.clone(),
        agent_name: execution.agent_name.clone().unwrap(),
    };
    startup_registry_record_start(&execution, task);
    let id = execution.execution_id.clone();
    let result = launch_task_execution_started(ctx, task, state.clone(), resolved, execution).await;
    match &result {
        Ok(_) => startup_registry_finish(&id, "completed", None),
        Err(error) => startup_registry_finish(&id, "failed", Some(error.clone())),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_resume_preserves_flags_and_uses_explicit_ids() {
        for (kind, flag) in [
            ("codex", "resume"),
            ("claude", "--resume"),
            ("opencode", "--session"),
            ("kimi", "--session"),
        ] {
            let original = vec!["--model".to_owned(), "custom".to_owned()];
            let args = resume_args(kind, &original, "native-1").unwrap();
            assert_eq!(args, ["--model", "custom", flag, "native-1"]);
            assert_eq!(
                resume_args(kind, &args, "native-2").unwrap(),
                ["--model", "custom", flag, "native-2"]
            );
        }
        assert!(resume_args("shell", &[], "id").is_err());
        assert!(resume_args("codex", &[], "--last").is_err());
    }
}
