//! Plan-owned spaces, fixed group tabs, and fresh agent panes.
use super::*;
use taskr_core::orchestration::{PlanLayout, PlanTab};

/// Keep agent terminals readable. A third concurrent worker opens another tab.
pub(super) const MAX_PANES_PER_TAB: usize = 2;
static LOCKS: LazyLock<Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Labels and native slash commands must stay on one line. Ownership uses IDs.
pub(super) fn display_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(100)
        .collect()
}

fn layout_key(plan: &PlanId, endpoint: &str) -> String {
    serde_json::to_string(&(plan, endpoint)).expect("layout key is serializable")
}

fn allocation_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = LOCKS.lock().unwrap();
    locks.retain(|_, value| value.strong_count() > 0);
    if let Some(lock) = locks.get(key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key.to_owned(), Arc::downgrade(&lock));
    lock
}

pub(super) fn lock_for(plan: &PlanId, endpoint: &str) -> Arc<tokio::sync::Mutex<()>> {
    allocation_lock(&layout_key(plan, endpoint))
}

/// A split target must be an actual recorded terminal, not a reused public ID.
fn owned_pane<'a>(
    state: &'a OrchestrationState,
    pane: &taskr_herdr::PaneInfo,
) -> Option<&'a TaskExecution> {
    state
        .tasks
        .values()
        .filter_map(|task| task.execution.as_ref())
        .chain(state.retained_executions.values().map(|row| &row.execution))
        .find(|execution| {
            !execution.pane_closed
                && execution.terminal_id.is_some()
                && execution.pane_id.as_deref() == Some(pane.pane_id.as_str())
                && execution.terminal_id.as_deref() == pane.terminal_id.as_deref()
        })
}

pub(super) async fn allocate(
    ctx: &LaunchContext,
    task: &Task,
    execution: &TaskExecution,
    resolved: &ResolvedLaunch,
) -> Result<taskr_herdr::AllocatedPane, taskr_herdr::RuntimeError> {
    let key = layout_key(&task.plan_id, &resolved.endpoint_id);
    let lock = allocation_lock(&key);
    let _guard = lock.lock().await;
    let state = ctx.orchestration.snapshot().map_err(preflight_error)?;
    let plan = state
        .plans
        .get(&task.plan_id)
        .ok_or_else(|| preflight_error("plan removed before allocation"))?;
    let target = ctx
        .herdr
        .target_for_endpoint(&resolved.endpoint_id)
        .fenced(execution.runtime_generation.clone());
    // Fail closed on incomplete inventories. Never allocate a duplicate space
    // simply because the existing endpoint cannot currently be inspected.
    let spaces = ctx
        .herdr
        .workspace_list(&target)
        .await
        .map_err(|e| e.with_certainty(taskr_herdr::DeliveryCertainty::NotDelivered))?;
    let panes = ctx
        .herdr
        .panes(&target)
        .await
        .map_err(|e| e.with_certainty(taskr_herdr::DeliveryCertainty::NotDelivered))?;
    let tabs = ctx
        .herdr
        .tabs(&target)
        .await
        .map_err(|e| e.with_certainty(taskr_herdr::DeliveryCertainty::NotDelivered))?;
    let mut layout = state.plan_layouts.get(&key).cloned().filter(|layout| {
        (taskr_core::coordination::generations_match(
            layout.runtime_generation.as_deref(),
            execution.runtime_generation.as_deref(),
        ) || (layout.runtime_generation.is_none()
            && execution.runtime_generation.is_none()
            && panes.iter().any(|pane| {
                pane.workspace_id.as_deref() == Some(&layout.workspace_id)
                    && owned_pane(&state, pane)
                        .is_some_and(|owner| owner.endpoint_id == execution.endpoint_id)
            })))
            && spaces
                .iter()
                .any(|space| space.workspace_id == layout.workspace_id)
    });
    if let Some(layout) = layout.as_mut() {
        layout.tabs.retain(|tab| {
            tabs.iter().any(|live| {
                Some(live.tab_id.as_str()) == Some(tab.tab_id.as_str())
                    && Some(live.workspace_id.as_str()) == Some(layout.workspace_id.as_str())
            })
        });
    }
    let placement = taskr_core::coordination::select_placement(
        layout.as_ref(),
        execution.group,
        &panes
            .iter()
            .map(|pane| taskr_core::coordination::LayoutPane {
                pane_id: pane.pane_id.clone(),
                workspace_id: pane.workspace_id.clone().unwrap_or_default(),
                tab_id: pane.tab_id.clone().unwrap_or_default(),
                owned_by_group: owned_pane(&state, pane).is_some_and(|owner| {
                    owner.endpoint_id == execution.endpoint_id
                        && owner.group == execution.group
                        && owner.runtime_generation == execution.runtime_generation
                }),
            })
            .collect::<Vec<_>>(),
        MAX_PANES_PER_TAB,
    );
    let split = match &placement {
        taskr_core::coordination::LayoutPlacement::Split { pane_id } => panes
            .iter()
            .find(|pane| &pane.pane_id == pane_id)
            .and_then(|pane| Some((pane.tab_id.clone()?, pane_id.clone()))),
        _ => None,
    };
    let number = layout
        .as_ref()
        .map(|layout| {
            layout
                .tabs
                .iter()
                .filter(|tab| tab.group == execution.group)
                .map(|tab| tab.ordinal)
                .max()
                .unwrap_or(0)
                + 1
        })
        .unwrap_or(1);
    let tab_label = if number == 1 {
        execution.group.label().to_owned()
    } else {
        format!("{} {number}", execution.group.label())
    };
    let placement = match placement {
        taskr_core::coordination::LayoutPlacement::Split { pane_id } => PanePlacement::Split {
            pane_id,
            direction: taskr_herdr::SplitDirection::Right,
            ratio: Some(0.5),
        },
        taskr_core::coordination::LayoutPlacement::NewTab { workspace_id } => {
            PanePlacement::NewTab { workspace_id }
        }
        taskr_core::coordination::LayoutPlacement::NewSpace => PanePlacement::NewWorkspace,
    };
    let mut allocated = ctx
        .herdr
        .allocate_pane(
            &target,
            &AllocatePaneRequest {
                cwd: resolved.workspace_path.clone(),
                env: resolved.env.clone(),
                label: if split.is_some() {
                    None
                } else {
                    Some(tab_label.clone())
                },
                workspace_label: Some(format!("{} · {}", plan.id.0, display_title(&plan.title))),
                placement,
            },
        )
        .await?;
    if let Some(layout) = &layout {
        allocated
            .workspace_id
            .get_or_insert(layout.workspace_id.clone());
    }
    if let Some((tab_id, _)) = &split {
        allocated.tab_id.get_or_insert(tab_id.clone());
    }
    let workspace_id = allocated
        .workspace_id
        .clone()
        .ok_or_else(|| allocation_error("allocation omitted workspace ID"))?;
    let tab_id = allocated
        .tab_id
        .clone()
        .ok_or_else(|| allocation_error("allocation omitted tab ID"))?;
    let layout = layout.get_or_insert(PlanLayout {
        plan_id: task.plan_id.clone(),
        endpoint_id: execution.endpoint_id.clone(),
        runtime_generation: execution.runtime_generation.clone(),
        workspace_id,
        tabs: Vec::new(),
    });
    if !layout.tabs.iter().any(|tab| tab.tab_id == tab_id) {
        layout.tabs.push(PlanTab {
            tab_id: tab_id.clone(),
            group: execution.group,
            ordinal: number,
        });
    }
    // Record the actual terminal before further fallible calls. Cancellation
    // can prevent agent startup, but must never lose allocated ownership.
    let mut bound = execution.clone();
    bound.workspace_id = allocated.workspace_id.clone();
    bound.tab_id = allocated.tab_id.clone();
    bound.pane_id = Some(allocated.pane_id.clone());
    bound.terminal_id = allocated.terminal_id.clone();
    ctx.orchestration
        .record_execution_if_current(task.id.clone(), bound)
        .map_err(allocation_error)?;
    ctx.orchestration
        .record_plan_layout(key, layout.clone())
        .map_err(allocation_error)?;
    if allocated.created_workspace {
        if let Err(error) = ctx
            .herdr
            .rename_layout(&target, "tab", &tab_id, &tab_label)
            .await
        {
            eprintln!("could not label new group tab: {error}");
        }
    }
    if let Err(error) = ctx
        .herdr
        .rename_layout(
            &target,
            "pane",
            &allocated.pane_id,
            &format!("{} · {}", task.id.0, display_title(&task.title)),
        )
        .await
    {
        eprintln!("could not label task pane: {error}");
    }
    Ok(allocated)
}

fn preflight_error(detail: impl Into<String>) -> taskr_herdr::RuntimeError {
    taskr_herdr::RuntimeError::invalid_launch_config("allocate_preflight", detail)
}
fn allocation_error(detail: impl Into<String>) -> taskr_herdr::RuntimeError {
    taskr_herdr::RuntimeError::new(
        taskr_herdr::RuntimeErrorCategory::UnknownOutcome,
        "allocate_persist",
        detail,
    )
    .with_certainty(taskr_herdr::DeliveryCertainty::Delivered)
}
