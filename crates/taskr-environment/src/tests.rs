use super::*;
use std::os::unix::fs::PermissionsExt;
use taskr_herdr::HerdrClientConfig;

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    config: CompanionConfig,
    herdr: HerdrClient,
}
impl Fixture {
    fn new() -> Self {
        // macOS exposes its temporary directory through /var -> /private/var.
        // Fixture paths must satisfy the same no-symlink-root contract as deployments.
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("taskr-environment-test-{}", uuid::Uuid::new_v4()));
        let source = root.join("user/.codex");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("config.toml"), "model = 'fixture-model'\n").unwrap();
        std::fs::write(
            source.join("review.config.toml"),
            "model = 'fixture-review'\n",
        )
        .unwrap();
        std::fs::write(
            source.join("auth.json"),
            r#"{"token":"fixture-native-login-never-persist"}"#,
        )
        .unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        write_executable(
            &bin.join("codex"),
            "#!/bin/sh\nif [ \"$1\" = --help ]; then printf '%s\\n' '--profile --no-daemon --no-alt-screen'; else printf 'codex-cli 0.160.0\\n'; fi\n",
        );
        let herdr_bin = bin.join("herdr");
        write_executable(&herdr_bin, "#!/bin/sh\nprintf '%s\\n' '[{\"id\":\"remote-a\",\"target\":\"fixture-worker\",\"enabled\":true},{\"id\":\"disabled\",\"target\":\"bad-worker\",\"enabled\":false}]'\n");
        let ssh = bin.join("ssh");
        // Runs the embedded companion in a fixture endpoint, with the exact
        // OpenSSH argument/stdio contract. No network or real agent starts.
        write_executable(
            &ssh,
            r#"#!/usr/bin/python3
import json, os, subprocess, sys
with open(os.environ['SSH_ARG_LOG'], 'a') as log:
    log.write(json.dumps(sys.argv[1:]) + '\n')
sys.exit(subprocess.call(['/bin/sh', '-c', sys.argv[-1]]))
"#,
        );
        let config = CompanionConfig {
            ssh_bin: ssh,
            environment: BTreeMap::from([
                (
                    "PATH".into(),
                    format!(
                        "{}:{}",
                        bin.display(),
                        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
                    ),
                ),
                ("HOME".into(), root.join("user").display().to_string()),
                (
                    "SSH_ARG_LOG".into(),
                    root.join("ssh-args.jsonl").display().to_string(),
                ),
            ]),
            ..CompanionConfig::default()
        };
        let herdr = HerdrClient::new(HerdrClientConfig {
            bin: herdr_bin,
            local_session: None,
        });
        Self {
            root,
            source,
            config,
            herdr,
        }
    }
    fn catalog(&self) -> Arc<EnvironmentCatalog> {
        let store = self.root.join("store");
        std::fs::create_dir_all(&store).unwrap();
        EnvironmentCatalog::open(&store, self.config.clone()).unwrap()
    }
    async fn discover(&self, catalog: &EnvironmentCatalog) -> AgentEnvironment {
        let result = catalog
            .discover(DiscoveryRequest {
                homes: Some(vec![self.source.display().to_string()]),
                source_path: None,
            })
            .await
            .unwrap();
        assert!(result.issues.is_empty());
        result.environments.into_iter().next().unwrap()
    }
    fn request(&self, source: &AgentEnvironment, endpoint: &str) -> SyncRequest {
        SyncRequest {
            source_environment_id: source.source_environment_id.clone(),
            source_revision: source.source_revision.clone(),
            endpoint_id: endpoint.into(),
            credential_policy: "copy".into(),
            endpoint_auth_home: None,
            deployment_root: Some(self.root.join("deployments").display().to_string()),
            dry_run: false,
            refresh: false,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn write_executable(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
async fn settled(catalog: &EnvironmentCatalog, id: &str) -> SyncJob {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let job = catalog.status(id).unwrap();
            if !matches!(job.state.as_str(), "queued" | "preparing") {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[test]
fn python_bundle_contract_tests_are_part_of_workspace_checks() {
    let result = std::process::Command::new("python3")
        .args([
            "-m",
            "unittest",
            "discover",
            "-s",
            "tests",
            "-p",
            "test_*.py",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
async fn local_sync_persists_ready_native_choices_without_login_contents() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = fixture.discover(&catalog).await;
    let request = fixture.request(&source, "local");
    let job = catalog
        .sync(fixture.herdr.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(job.state, "queued");
    let ready = settled(&catalog, &job.sync_job_id).await;
    assert_eq!(ready.state, "ready", "{:?}", ready.error);
    assert_eq!(ready.launch_profile_ids.len(), 2);
    assert_eq!(
        catalog
            .sync(fixture.herdr.clone(), request)
            .await
            .unwrap()
            .sync_job_id,
        ready.sync_job_id
    );
    let choices = catalog.list("local").unwrap();
    assert_eq!(choices[0].native_profile, None);
    assert_eq!(choices[1].native_profile.as_deref(), Some("review"));
    assert!(choices[1]
        .profile
        .args
        .windows(2)
        .any(|args| args == ["--profile", "review"]));
    assert!(!choices[1].profile.args.iter().any(|a| a.contains("bypass")));
    catalog
        .verify(&fixture.herdr, "local", &choices[0].profile.id)
        .await
        .unwrap();
    assert!(catalog
        .choice("remote-a", Some(&choices[0].profile.id))
        .is_err());
    assert!(catalog.choice("local", None).is_err());
    let reopened = fixture.catalog();
    assert_eq!(reopened.list("local").unwrap().len(), 2);
    assert_eq!(reopened.status(&ready.sync_job_id).unwrap().state, "ready");
    let bytes = std::fs::read(fixture.root.join("store/taskr.db")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("fixture-native-login-never-persist"));
}

#[tokio::test]
async fn changed_sources_fail_and_new_revisions_preserve_old_choices() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = fixture.discover(&catalog).await;
    std::fs::write(fixture.source.join("config.toml"), "model='new-model'\n").unwrap();
    let job = catalog
        .sync(fixture.herdr.clone(), fixture.request(&source, "local"))
        .await
        .unwrap();
    assert_eq!(settled(&catalog, &job.sync_job_id).await.state, "failed");
    assert!(catalog.list("local").unwrap().is_empty());
    let source = fixture.discover(&catalog).await;
    let job = catalog
        .sync(fixture.herdr.clone(), fixture.request(&source, "local"))
        .await
        .unwrap();
    let first = settled(&catalog, &job.sync_job_id).await;
    assert_eq!(first.state, "ready", "{:?}", first.error);
    std::fs::write(fixture.source.join("config.toml"), "model='next-model'\n").unwrap();
    let source = fixture.discover(&catalog).await;
    let job = catalog
        .sync(fixture.herdr.clone(), fixture.request(&source, "local"))
        .await
        .unwrap();
    let second = settled(&catalog, &job.sync_job_id).await;
    assert_eq!(second.state, "ready", "{:?}", second.error);
    assert_ne!(
        first.prepared.unwrap().deployment_id,
        second.prepared.unwrap().deployment_id
    );
    assert_eq!(catalog.list("local").unwrap().len(), 4);
}

#[tokio::test]
async fn explicit_refresh_rotates_login_without_redirecting_old_choices() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = fixture.discover(&catalog).await;
    let request = fixture.request(&source, "local");
    let job = catalog
        .sync(fixture.herdr.clone(), request.clone())
        .await
        .unwrap();
    let first = settled(&catalog, &job.sync_job_id).await;
    assert_eq!(first.state, "ready", "{:?}", first.error);
    let old_home = first.prepared.unwrap().home;
    std::fs::write(
        fixture.source.join("auth.json"),
        r#"{"token":"rotated-login"}"#,
    )
    .unwrap();
    let mut refresh = request;
    refresh.refresh = true;
    let job = catalog.sync(fixture.herdr.clone(), refresh).await.unwrap();
    let next = settled(&catalog, &job.sync_job_id).await;
    assert_eq!(next.state, "ready", "{:?}", next.error);
    assert_ne!(old_home, next.prepared.unwrap().home);
    assert!(
        std::fs::read_to_string(Path::new(&old_home).join("auth.json"))
            .unwrap()
            .contains("fixture-native-login-never-persist")
    );
    assert_eq!(catalog.list("local").unwrap().len(), 4);
    catalog
        .verify(&fixture.herdr, "local", &first.launch_profile_ids[0])
        .await
        .unwrap();
}

#[tokio::test]
async fn dry_run_and_canceled_jobs_never_create_launch_choices() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = fixture.discover(&catalog).await;
    let mut request = fixture.request(&source, "local");
    request.dry_run = true;
    let job = catalog.sync(fixture.herdr.clone(), request).await.unwrap();
    let ready = settled(&catalog, &job.sync_job_id).await;
    assert_eq!(ready.state, "ready", "{:?}", ready.error);
    assert!(ready.launch_profile_ids.is_empty());
    assert!(!Path::new(&ready.prepared.unwrap().home).exists());
    let job = catalog
        .sync(fixture.herdr.clone(), fixture.request(&source, "local"))
        .await
        .unwrap();
    assert_eq!(catalog.cancel(&job.sync_job_id).unwrap().state, "canceled");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(catalog.list("local").unwrap().is_empty());
    assert_eq!(
        fixture.catalog().status(&job.sync_job_id).unwrap().state,
        "canceled"
    );
}

#[tokio::test]
async fn restart_reconciles_pending_job_into_one_ready_deployment() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = fixture.discover(&catalog).await;
    let id = "sync-restarted".to_owned();
    catalog
        .update(|registry| {
            registry.jobs.insert(
                id.clone(),
                SyncJob {
                    sync_job_id: id.clone(),
                    state: "preparing".into(),
                    request: fixture.request(&source, "local"),
                    source,
                    error: None,
                    prepared: None,
                    launch_profile_ids: Vec::new(),
                },
            );
            Ok(())
        })
        .unwrap();
    drop(catalog);
    let reopened = fixture.catalog();
    reopened.resume_pending(fixture.herdr.clone()).unwrap();
    let ready = settled(&reopened, &id).await;
    assert_eq!(ready.state, "ready", "{:?}", ready.error);
    assert_eq!(reopened.list("local").unwrap().len(), 2);
}

#[tokio::test]
async fn remote_sync_uses_only_enabled_saved_herdr_target_and_stdin_payload() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = fixture.discover(&catalog).await;
    for endpoint in ["disabled", "unknown"] {
        assert!(catalog
            .sync(fixture.herdr.clone(), fixture.request(&source, endpoint))
            .await
            .is_err());
    }
    assert!(!fixture.root.join("ssh-args.jsonl").exists());
    let job = catalog
        .sync(fixture.herdr.clone(), fixture.request(&source, "remote-a"))
        .await
        .unwrap();
    let ready = settled(&catalog, &job.sync_job_id).await;
    assert_eq!(ready.state, "ready", "{:?}", ready.error);
    assert!(catalog.list("local").unwrap().is_empty());
    assert_eq!(catalog.list("remote-a").unwrap().len(), 2);
    let args = std::fs::read_to_string(fixture.root.join("ssh-args.jsonl")).unwrap();
    assert!(args.contains("fixture-worker"));
    assert!(!args.contains("fixture-native-login-never-persist"));
    let args: Vec<String> = serde_json::from_str(args.lines().next().unwrap()).unwrap();
    assert_eq!(
        &args[..6],
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "--",
            "fixture-worker",
        ]
    );
    assert!(args[6].starts_with("env "));
    assert!(args[6].contains("'python3' '-c' '"));
}

#[tokio::test]
async fn folder_import_syncs_profiles_without_controller_user_skills_and_persists_provenance() {
    let fixture = Fixture::new();
    let package = fixture.root.join("package/work");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("config.toml"), "model='import-base'\n").unwrap();
    std::fs::write(
        package.join("review.config.toml"),
        "model='import-review'\n",
    )
    .unwrap();
    std::fs::write(
        package.join("auth.json"),
        r#"{"token":"import-login-never-persist"}"#,
    )
    .unwrap();
    let ambient = fixture.root.join("user/.agents/skills/ambient");
    std::fs::create_dir_all(&ambient).unwrap();
    std::fs::write(ambient.join("SKILL.md"), "ambient private instructions").unwrap();
    let catalog = fixture.catalog();
    let discovered = catalog
        .discover(DiscoveryRequest {
            homes: None,
            source_path: Some(fixture.root.join("package").display().to_string()),
        })
        .await
        .unwrap();
    assert!(discovered.issues.is_empty());
    let source = discovered.environments[0].clone();
    assert_eq!(source.source_location.as_ref().unwrap().kind, "folder");
    drop(catalog);
    let catalog = fixture.catalog();
    let queued = catalog
        .sync(fixture.herdr.clone(), fixture.request(&source, "remote-a"))
        .await
        .unwrap();
    let ready = settled(&catalog, &queued.sync_job_id).await;
    assert_eq!(ready.state, "ready", "{:?}", ready.error);
    assert_eq!(ready.launch_profile_ids.len(), 2);
    let home = PathBuf::from(ready.prepared.unwrap().home);
    assert!(home.join("review.config.toml").is_file());
    assert!(!home.join("skills/_user_agents/ambient").exists());
    let raw = std::fs::read(fixture.root.join("store/taskr.db")).unwrap();
    let raw = String::from_utf8_lossy(&raw);
    assert!(!raw.contains("import-login-never-persist"));
    assert!(!raw.contains("import-base"));
}

#[tokio::test]
async fn discovery_rejects_conflicting_and_empty_sources_before_companion_io() {
    let catalog = EnvironmentCatalog::in_memory(
        vec![],
        CompanionConfig {
            python_bin: "/must-not-run".into(),
            ..CompanionConfig::default()
        },
    );
    let error = catalog
        .discover(DiscoveryRequest {
            homes: Some(vec![]),
            source_path: Some("/package".into()),
        })
        .await
        .unwrap_err();
    assert!(error.contains("mutually exclusive"));
    let error = catalog
        .discover(DiscoveryRequest {
            homes: None,
            source_path: Some(" ".into()),
        })
        .await
        .unwrap_err();
    assert!(error.contains("empty"));
}

#[test]
fn invalid_policy_and_relative_endpoint_paths_are_rejected() {
    let fixture = Fixture::new();
    let source = AgentEnvironment {
        source_environment_id: "env-x".into(),
        source_revision: "r".into(),
        source_home: "/source".into(),
        display_name: "codex / source".into(),
        kind: "codex".into(),
        cli_version: "0.160.0".into(),
        native_profiles: vec![],
        source_location: None,
    };
    let mut request = fixture.request(&source, "local");
    request.credential_policy = "implicit".into();
    assert!(validate_sync(&request).is_err());
    request.credential_policy = "endpoint".into();
    assert!(validate_sync(&request).is_err());
    request.endpoint_auth_home = Some("relative/home".into());
    assert!(validate_sync(&request).is_err());
}
