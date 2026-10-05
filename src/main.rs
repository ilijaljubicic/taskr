use std::ffi::OsString;
use std::path::PathBuf;

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.len() == 1
        || matches!(
            args.get(1).and_then(|arg| arg.to_str()),
            Some("-h" | "--help")
        )
    {
        print_root_help();
        return;
    }

    let (store_path, rest) = peel_leading_store_path(&args[1..]);
    let Some(command) = rest.first().and_then(|arg| arg.to_str()) else {
        taskr_controller::main_entry_from(args);
        return;
    };
    let rest = &rest[1..];
    match command {
        "controller" => {
            let mut argv = vec![args[0].clone()];
            if let Some(path) = &store_path {
                argv.push(OsString::from("--store-path"));
                argv.push(path.into());
            }
            argv.extend_from_slice(rest);
            taskr_controller::main_entry_from(argv);
        }
        "list-projects" => {
            std::process::exit(run_list_projects(store_path));
        }
        "create-project" => {
            std::process::exit(run_create_project(store_path, rest));
        }
        "delete-project" => {
            std::process::exit(run_delete_project(store_path, rest));
        }
        "prune" => {
            std::process::exit(run_prune(store_path, rest));
        }
        _ => {
            taskr_controller::main_entry_from(args);
        }
    }
}

/// Peel `--store-path <path>` / `--store-path=<path>` when they appear as the
/// first arguments so every subcommand accepts a store override uniformly.
fn peel_leading_store_path(args: &[OsString]) -> (Option<PathBuf>, &[OsString]) {
    let mut index = 0;
    let mut store_path = None;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--store-path" {
            match args.get(index + 1) {
                Some(value) => {
                    store_path = Some(PathBuf::from(value));
                    index += 2;
                }
                None => break,
            }
            continue;
        }
        match arg
            .to_str()
            .and_then(|text| text.strip_prefix("--store-path="))
        {
            Some(value) => {
                store_path = Some(PathBuf::from(value));
                index += 1;
            }
            None => break,
        }
    }
    (store_path, &args[index..])
}

fn print_root_help() {
    if let Err(error) = taskr_controller::print_help() {
        eprintln!("failed to print controller help: {error}");
    }
    println!();
    println!("Root commands:");
    println!("  controller              Run the MCP controller explicitly (default when no subcommand matches)");
    println!(
        "  create-project <title> --description <text>  Create a durable orchestration project in taskr.db"
    );
    println!(
        "  delete-project <id-or-slug>  Delete an orchestration project and its plans/tasks from taskr.db"
    );
    println!("  list-projects           List durable orchestration projects from taskr.db");
    println!(
        "  prune                   Remove aged retained terminals, stale execution records, and finished plans"
    );
}

type CreateProjectArgs = taskr_controller::CreateProject;

fn run_create_project(store_path: Option<PathBuf>, raw_args: &[OsString]) -> i32 {
    if raw_args
        .iter()
        .any(|arg| matches!(arg.to_str(), Some("-h" | "--help")))
    {
        print_create_project_help();
        return 0;
    }
    let args = match parse_create_project_args(raw_args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("taskr create-project: {error}");
            return 2;
        }
    };
    let store_path = match resolve_store(store_path.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("taskr create-project: {error}");
            return 2;
        }
    };
    let project = match taskr_controller::local_create_project(Some(&store_path), args) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("taskr create-project: {error}");
            return 1;
        }
    };
    println!(
        "{}\t{}\t{:?}\t{}/{}\t{}",
        project.id.0,
        project.slug,
        project.status,
        project.active_task_count,
        project.task_count,
        project.title
    );
    0
}

fn print_create_project_help() {
    println!("usage: taskr create-project <title> --description <text> [--slug <slug>] [--codex-home <path>] [--claude-home <path>] [--opencode-home <path>] [--kimi-home <path>]");
    println!("Optional homes are per-project configuration-home overrides applied on execution endpoints.");
    println!();
    println!("Creates a durable orchestration project in taskr.db.");
}

fn parse_create_project_args(raw_args: &[OsString]) -> Result<CreateProjectArgs, String> {
    let mut title = None;
    let mut description = None;
    let mut slug = None;
    let mut homes = CreateProjectArgs::default();
    let mut index = 0;

    while index < raw_args.len() {
        let text = raw_args[index]
            .to_str()
            .ok_or_else(|| "arguments must be valid UTF-8".to_owned())?;
        let (flag, inline_value) = text
            .split_once('=')
            .map_or((text, None), |(flag, value)| (flag, Some(value)));
        let home = match flag {
            "--codex-home" => Some(&mut homes.codex_home),
            "--claude-home" => Some(&mut homes.claude_home),
            "--opencode-home" => Some(&mut homes.opencode_home),
            "--kimi-home" => Some(&mut homes.kimi_home),
            _ => None,
        };
        if let Some(home) = home {
            let value = match inline_value {
                Some(value) => value,
                None => raw_args
                    .get(index + 1)
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.starts_with("--"))
                    .ok_or_else(|| format!("{flag} requires a UTF-8 path"))?,
            };
            *home = Some(value.to_owned());
            index += if inline_value.is_some() { 1 } else { 2 };
            continue;
        }
        match text {
            "--description" => {
                let value = raw_args
                    .get(index + 1)
                    .ok_or_else(|| "--description requires a value".to_owned())?
                    .to_str()
                    .ok_or_else(|| "--description value must be valid UTF-8".to_owned())?;
                description = Some(value.to_owned());
                index += 2;
            }
            "--slug" => {
                let value = raw_args
                    .get(index + 1)
                    .ok_or_else(|| "--slug requires a value".to_owned())?
                    .to_str()
                    .ok_or_else(|| "--slug value must be valid UTF-8".to_owned())?;
                slug = Some(value.to_owned());
                index += 2;
            }
            _ => {
                if let Some(value) = text.strip_prefix("--description=") {
                    description = Some(value.to_owned());
                    index += 1;
                } else if let Some(value) = text.strip_prefix("--slug=") {
                    slug = Some(value.to_owned());
                    index += 1;
                } else if text.starts_with('-') {
                    return Err(format!("unknown argument '{text}'"));
                } else if title.is_some() {
                    return Err("project title may only be provided once".into());
                } else {
                    title = Some(text.to_owned());
                    index += 1;
                }
            }
        }
    }

    let title = title.ok_or_else(|| "project title is required".to_owned())?;
    if title.trim().is_empty() {
        return Err("project title must not be empty".into());
    }
    let description = description.ok_or_else(|| "project description is required".to_owned())?;
    if description.trim().is_empty() {
        return Err("project description must not be empty".into());
    }
    Ok(CreateProjectArgs {
        title,
        description,
        slug,
        ..homes
    })
}

fn run_list_projects(store_path: Option<PathBuf>) -> i32 {
    let store_path = match resolve_store(store_path.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("taskr list-projects: {error}");
            return 2;
        }
    };
    let projects = match taskr_controller::local_projects(Some(&store_path)) {
        Ok(projects) => projects,
        Err(error) => {
            eprintln!("taskr list-projects: {error}");
            return 1;
        }
    };
    for project in projects {
        println!(
            "{}\t{}\t{:?}\t{}/{}\t{}",
            project.id.0,
            project.slug,
            project.status,
            project.active_task_count,
            project.task_count,
            project.title
        );
    }
    0
}

fn run_delete_project(store_path: Option<PathBuf>, raw_args: &[OsString]) -> i32 {
    if raw_args
        .iter()
        .any(|arg| matches!(arg.to_str(), Some("-h" | "--help")))
    {
        print_delete_project_help();
        return 0;
    }
    let project_id_or_slug = match parse_delete_project_args(raw_args) {
        Ok(project_id_or_slug) => project_id_or_slug,
        Err(error) => {
            eprintln!("taskr delete-project: {error}");
            return 2;
        }
    };
    let store_path = match resolve_store(store_path.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("taskr delete-project: {error}");
            return 2;
        }
    };
    let report =
        match taskr_controller::local_delete_project(Some(&store_path), &project_id_or_slug) {
            Ok(report) => report,
            Err(error) => {
                eprintln!("taskr delete-project: {error}");
                return 1;
            }
        };
    let project = report.project;
    println!(
        "deleted\t{}\t{}\t{:?}\tplans={}\ttasks={}\tedges={}\t{}",
        project.id.0,
        project.slug,
        project.status,
        report.deleted_plan_count,
        report.deleted_task_count,
        report.deleted_edge_count,
        project.title
    );
    0
}

fn print_delete_project_help() {
    println!("usage: taskr delete-project <id-or-slug>");
    println!();
    println!("Deletes an orchestration project from taskr.db.");
    println!("Also deletes all plans, task cards, and task edges in that project.");
}

fn parse_delete_project_args(raw_args: &[OsString]) -> Result<String, String> {
    let mut project_id_or_slug = None;
    for arg in raw_args {
        let text = arg
            .to_str()
            .ok_or_else(|| "arguments must be valid UTF-8".to_owned())?;
        if text.starts_with('-') {
            return Err(format!("unknown argument '{text}'"));
        }
        if project_id_or_slug.is_some() {
            return Err("project id or slug may only be provided once".into());
        }
        project_id_or_slug = Some(text.to_owned());
    }
    let project_id_or_slug =
        project_id_or_slug.ok_or_else(|| "project id or slug is required".to_owned())?;
    if project_id_or_slug.trim().is_empty() {
        return Err("project id or slug must not be empty".into());
    }
    Ok(project_id_or_slug)
}

fn resolve_store(store_path: Option<&std::path::Path>) -> Result<PathBuf, String> {
    taskr_controller::resolve_store_path(store_path)
}

const DEFAULT_PRUNE_OLDER_THAN_DAYS: u64 = 14;
const DEFAULT_HERDR_BIN: &str = "herdr";

#[derive(Clone, Debug, Eq, PartialEq)]
struct PruneInclude {
    stale_execution_records: bool,
    finished_plans: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PruneArgs {
    dry_run: bool,
    older_than_days: u64,
    herdr_bin: String,
    herdr_session: Option<String>,
    include: PruneInclude,
}

fn run_prune(store_path: Option<PathBuf>, raw_args: &[OsString]) -> i32 {
    if raw_args
        .iter()
        .any(|arg| matches!(arg.to_str(), Some("-h" | "--help")))
    {
        print_prune_help();
        return 0;
    }
    let args = match parse_prune_args(raw_args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("taskr prune: {error}");
            return 2;
        }
    };
    let store_path = match resolve_store(store_path.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("taskr prune: {error}");
            return 2;
        }
    };
    let report = match taskr_controller::local_prune_store_blocking(
        Some(&store_path),
        &args.herdr_bin,
        args.herdr_session.as_deref(),
        args.dry_run,
        args.include.stale_execution_records,
        args.include.finished_plans,
        Some(args.older_than_days),
    ) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("taskr prune: {error}");
            return 1;
        }
    };
    for warning in &report.endpoint_warnings {
        eprintln!("taskr prune: endpoint warning: {warning}");
    }
    if report.dry_run {
        println!(
            "dry-run: would prune {} stale execution record(s) and {} finished plan(s) after observing {} endpoint(s)",
            report.pruned_execution_count,
            report.pruned_plan_count,
            report.endpoints_observed.len()
        );
    } else {
        println!(
            "pruned {} stale execution record(s) and {} finished plan(s) after observing {} endpoint(s)",
            report.pruned_execution_count,
            report.pruned_plan_count,
            report.endpoints_observed.len()
        );
    }
    for candidate in report.candidates {
        println!(
            "{}\tlast_seen_ms={}\ttask={}\t{}",
            candidate.runtime_key, candidate.last_seen_ms, candidate.task_id, candidate.reason
        );
    }
    0
}

fn print_prune_help() {
    println!("usage: taskr prune [--dry-run|--execute] [--older-than-days <days>] [--include-stale-execution-records] [--include-finished-plans] [--herdr-bin <path>] [--herdr-session <id>]");
    println!();
    println!("Observes Herdr endpoints, then removes aged retained shell panes, stale execution records, and finished plans.");
    println!("Retained panes require matching immutable terminal identity and an available shell; reused or unreachable resources are kept.");
    println!("Defaults: dry-run, all include categories enabled, --older-than-days 14.");
    println!("Pass one or more --include-* flags to scope pruning to only those categories.");
    println!("Use --execute to remove selected retained panes and update taskr.db.");
}

fn parse_prune_args(raw_args: &[OsString]) -> Result<PruneArgs, String> {
    let mut dry_run = true;
    let mut older_than_days = DEFAULT_PRUNE_OLDER_THAN_DAYS;
    let mut herdr_bin = DEFAULT_HERDR_BIN.to_owned();
    let mut herdr_session = None;
    let mut include_stale_execution_records = false;
    let mut include_finished_plans = false;
    let mut index = 0;

    while index < raw_args.len() {
        let text = raw_args[index]
            .to_str()
            .ok_or_else(|| "arguments must be valid UTF-8".to_owned())?;
        match text {
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--execute" => {
                dry_run = false;
                index += 1;
            }
            "--include-stale-execution-records" => {
                include_stale_execution_records = true;
                index += 1;
            }
            "--include-finished-plans" => {
                include_finished_plans = true;
                index += 1;
            }
            "--herdr-bin" => {
                let value = raw_args
                    .get(index + 1)
                    .ok_or_else(|| "--herdr-bin requires a value".to_owned())?
                    .to_str()
                    .ok_or_else(|| "--herdr-bin value must be valid UTF-8".to_owned())?;
                herdr_bin = value.to_owned();
                index += 2;
            }
            "--herdr-session" => {
                let value = raw_args
                    .get(index + 1)
                    .ok_or_else(|| "--herdr-session requires a value".to_owned())?
                    .to_str()
                    .ok_or_else(|| "--herdr-session value must be valid UTF-8".to_owned())?;
                herdr_session = Some(value.to_owned());
                index += 2;
            }
            "--older-than-days" => {
                let value = raw_args
                    .get(index + 1)
                    .ok_or_else(|| "--older-than-days requires a value".to_owned())?
                    .to_str()
                    .ok_or_else(|| "--older-than-days value must be valid UTF-8".to_owned())?;
                older_than_days = parse_days(value)?;
                index += 2;
            }
            _ => {
                if let Some(value) = text.strip_prefix("--older-than-days=") {
                    older_than_days = parse_days(value)?;
                    index += 1;
                } else if let Some(value) = text.strip_prefix("--herdr-bin=") {
                    herdr_bin = value.to_owned();
                    index += 1;
                } else if let Some(value) = text.strip_prefix("--herdr-session=") {
                    herdr_session = Some(value.to_owned());
                    index += 1;
                } else {
                    return Err(format!("unknown argument '{text}'"));
                }
            }
        }
    }

    let any_include = include_stale_execution_records || include_finished_plans;
    let include = if any_include {
        PruneInclude {
            stale_execution_records: include_stale_execution_records,
            finished_plans: include_finished_plans,
        }
    } else {
        PruneInclude {
            stale_execution_records: true,
            finished_plans: true,
        }
    };

    Ok(PruneArgs {
        dry_run,
        older_than_days,
        herdr_bin,
        herdr_session,
        include,
    })
}

fn parse_days(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid --older-than-days value '{value}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn create_project_args_accept_title_description_and_optional_slug() {
        let args = parse_create_project_args(&os_args(&[
            "My Project",
            "--description",
            "Long-running work",
            "--slug=custom-project",
        ]))
        .expect("create project args");

        assert_eq!(
            args,
            CreateProjectArgs {
                title: "My Project".into(),
                description: "Long-running work".into(),
                slug: Some("custom-project".into()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn create_project_args_accept_optional_coder_homes() {
        let args = parse_create_project_args(&os_args(&[
            "Homes",
            "--description",
            "Per-project configuration",
            "--codex-home",
            "/node/codex",
            "--claude-home=/node/claude",
            "--opencode-home",
            "/node/opencode",
            "--kimi-home=~/kimi",
        ]))
        .unwrap();
        assert_eq!(args.codex_home.as_deref(), Some("/node/codex"));
        assert_eq!(args.claude_home.as_deref(), Some("/node/claude"));
        assert_eq!(args.opencode_home.as_deref(), Some("/node/opencode"));
        assert_eq!(args.kimi_home.as_deref(), Some("~/kimi"));
        assert!(parse_create_project_args(&os_args(&[
            "Homes",
            "--description",
            "Test",
            "--codex-home"
        ]))
        .is_err());
    }

    #[test]
    fn create_project_args_require_title_and_description() {
        let error = parse_create_project_args(&os_args(&[])).expect_err("missing title");
        assert_eq!(error, "project title is required");

        let error =
            parse_create_project_args(&os_args(&["My Project"])).expect_err("missing description");
        assert_eq!(error, "project description is required");

        let error =
            parse_create_project_args(&os_args(&["One", "Two"])).expect_err("duplicate title");
        assert_eq!(error, "project title may only be provided once");
    }

    #[test]
    fn peel_leading_store_path_supports_both_forms() {
        let first = os_args(&["--store-path", "/tmp/store", "prune", "--dry-run"]);
        let (store, rest) = peel_leading_store_path(&first);
        assert_eq!(store, Some(PathBuf::from("/tmp/store")));
        assert_eq!(rest, os_args(&["prune", "--dry-run"]));

        let second = os_args(&["--store-path=/tmp/other", "create-project", "Demo"]);
        let (store, rest) = peel_leading_store_path(&second);
        assert_eq!(store, Some(PathBuf::from("/tmp/other")));
        assert_eq!(rest, os_args(&["create-project", "Demo"]));

        let third = os_args(&["list-projects"]);
        let (store, rest) = peel_leading_store_path(&third);
        assert_eq!(store, None);
        assert_eq!(rest, os_args(&["list-projects"]));
    }

    #[test]
    fn prune_args_parse_dry_run_include_and_age() {
        let args = parse_prune_args(&os_args(&[
            "--dry-run",
            "--include-stale-execution-records",
            "--older-than-days",
            "7",
        ]))
        .unwrap();

        assert_eq!(
            args,
            PruneArgs {
                dry_run: true,
                older_than_days: 7,
                herdr_bin: DEFAULT_HERDR_BIN.to_owned(),
                herdr_session: None,
                include: PruneInclude {
                    stale_execution_records: true,
                    finished_plans: false,
                },
            }
        );
    }

    #[test]
    fn prune_args_default_to_dry_run_all_categories_and_fourteen_days() {
        let args = parse_prune_args(&os_args(&[])).unwrap();

        assert_eq!(
            args,
            PruneArgs {
                dry_run: true,
                older_than_days: 14,
                herdr_bin: DEFAULT_HERDR_BIN.to_owned(),
                herdr_session: None,
                include: PruneInclude {
                    stale_execution_records: true,
                    finished_plans: true,
                },
            }
        );
    }

    #[test]
    fn prune_args_accept_execute_equals_forms_and_herdr_flags() {
        let args = parse_prune_args(&os_args(&[
            "--execute",
            "--older-than-days=30",
            "--herdr-bin=/usr/local/bin/herdr",
            "--herdr-session",
            "main",
        ]))
        .unwrap();

        assert_eq!(
            args,
            PruneArgs {
                dry_run: false,
                older_than_days: 30,
                herdr_bin: "/usr/local/bin/herdr".to_owned(),
                herdr_session: Some("main".to_owned()),
                include: PruneInclude {
                    stale_execution_records: true,
                    finished_plans: true,
                },
            }
        );
    }

    #[test]
    fn prune_args_reject_unknown_flags() {
        let error = parse_prune_args(&os_args(&["--all"])).unwrap_err();

        assert!(error.contains("unknown argument"));
    }
}
