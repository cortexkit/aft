use super::*;

fn context(project: &Path) -> AppContext {
    AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.to_path_buf()),
            ..Default::default()
        },
    )
}

fn set_role_modes(ctx: &AppContext, mode: SandboxMode, worker: bool) {
    // The unselected role always has a different level.
    let other = if mode == SandboxMode::Off {
        SandboxMode::Standard
    } else {
        SandboxMode::Off
    };
    ctx.update_config(|config| {
        config.sandbox.profile.head = if worker { other } else { mode };
        config.sandbox.profile.worker = if worker { mode } else { other };
    });
}

fn resolve(ctx: &AppContext, principal: &AuthenticatedPrincipal, task_io: &Path) -> SpawnPlan {
    resolve_sandbox_spawn(
        ctx,
        principal,
        RequestedSandboxTier::Native,
        SandboxTaskKind::BashForeground,
        task_io,
        None,
    )
}

#[cfg(windows)]
#[test]
fn sandbox_level_policy_windows_selects_real_roles_and_refuses_forced_strict() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for worker in [false, true] {
        with_worker_jail(worker, || {
            for mode in [
                SandboxMode::Off,
                SandboxMode::Credentials,
                SandboxMode::Standard,
                SandboxMode::Strict,
            ] {
                set_role_modes(&ctx, mode, worker);
                let plan = resolve(&ctx, &AuthenticatedPrincipal::FirstParty, dir.path());
                match mode {
                    SandboxMode::Off => assert_eq!(plan, SpawnPlan::Unsandboxed),
                    SandboxMode::Credentials | SandboxMode::Standard => {
                        assert_eq!(
                            plan,
                            SpawnPlan::Unenforced {
                                mode,
                                worker_gh_fence: worker
                            }
                        );
                        for tier in [RequestedSandboxTier::Disabled, RequestedSandboxTier::Host] {
                            let bypass = resolve_sandbox_spawn(
                                &ctx,
                                &AuthenticatedPrincipal::FirstParty,
                                tier,
                                SandboxTaskKind::BashForeground,
                                dir.path(),
                                None,
                            );
                            assert_eq!(bypass.refusal_code(), Some("sandbox_unavailable"));
                        }
                    }
                    SandboxMode::Strict => {
                        assert_eq!(plan.refusal_code(), Some("sandbox_unavailable"))
                    }
                }
            }
            set_role_modes(&ctx, SandboxMode::Off, worker);
            let forced = AuthenticatedPrincipal::RouteBind {
                trust: PrincipalTrust::Untrusted,
                route_channel: 7,
                route_epoch: 1,
                project_root: dir.path().to_path_buf(),
                harness: "mcp:test".into(),
                session_id: "forced-strict".into(),
                principal_id: Some("unverified".into()),
            };
            for tier in [
                RequestedSandboxTier::Native,
                RequestedSandboxTier::Disabled,
                RequestedSandboxTier::Host,
            ] {
                let plan = resolve_sandbox_spawn(
                    &ctx,
                    &forced,
                    tier,
                    SandboxTaskKind::BashForeground,
                    dir.path(),
                    None,
                );
                assert_eq!(plan.refusal_code(), Some("sandbox_unavailable"));
                assert_eq!(plan.refusal_message(), Some("sandbox profile strict is not enforced on Windows; select sandbox.profile off or run on macOS/Linux"));
            }
        });
    }
}

#[cfg(unix)]
thread_local! {
    static ACCOUNT_HOME_OVERRIDE: RefCell<Option<std::io::Result<PathBuf>>> = const { RefCell::new(None) };
}

#[cfg(unix)]
pub(super) fn account_home_override() -> Option<std::io::Result<PathBuf>> {
    ACCOUNT_HOME_OVERRIDE.with(|value| {
        value.borrow().as_ref().map(|result| {
            result
                .as_ref()
                .cloned()
                .map_err(|error| std::io::Error::other(error.to_string()))
        })
    })
}

#[cfg(unix)]
fn unix_fixture(base: &Path) -> (AppContext, PathBuf, PathBuf, impl Drop) {
    let project = base.join("checkout");
    let store = base.join("store");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&store).unwrap();
    let ctx = context(&project);
    ctx.update_config(|config| {
        config.storage_dir = Some(store.clone());
        config.github.shim = false;
        config.git.co_author = "off".into();
        config.sandbox.read_deny = vec![base.join("private-notes")];
    });
    let task = crate::bash_background::persistence::create_task_layout(
        &store,
        "sandbox-level-test",
        "bash-0123456789abcdef",
    )
    .unwrap();
    let launcher = base.join("aft-test-launcher");
    std::fs::write(&launcher, "launcher fixture").unwrap();
    let fixture = install_sandbox_launcher_for_test(&project, launcher);
    (ctx, project, task.paths.io_dir, fixture)
}

#[cfg(unix)]
#[test]
fn sandbox_level_policy_refuses_unresolved_account_home_for_credentials_head() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, _, io, _fixture) = unix_fixture(dir.path());
    set_role_modes(&ctx, SandboxMode::Credentials, false);
    ACCOUNT_HOME_OVERRIDE.with(|value| {
        value.replace(Some(Err(std::io::Error::other(
            "test passwd lookup failed",
        ))));
        let plan = resolve(&ctx, &AuthenticatedPrincipal::FirstParty, &io);
        value.replace(None);
        assert_eq!(plan.refusal_code(), Some("sandbox_unavailable"));
        assert!(plan.refusal_message().unwrap().contains(
            "cannot resolve account home for the credential floor: test passwd lookup failed"
        ));
    });
}

#[cfg(target_os = "linux")]
#[test]
fn sandbox_level_policy_required_plan_refusal_names_the_secret_requirement() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, _, io, _fixture) = unix_fixture(dir.path());
    let requirement = SandboxRequirement::for_command("echo '{{ck-secret:test}}'").unwrap();
    let rejected = SpawnPlan::Refused {
        code: "sandbox_unavailable",
        message: "test launcher unavailable".into(),
        mismatch_class: None,
    };
    let plan = with_spawn_plan_for_test(rejected, || {
        resolve_command_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            &io,
            None,
            Some(requirement),
        )
    });
    assert_eq!(plan.refusal_code(), Some("secrets_sandbox_required"));
    assert!(plan
        .refusal_message()
        .unwrap()
        .contains("test launcher unavailable"));
}

#[cfg(unix)]
#[test]
fn sandbox_level_policy_selects_real_roles_and_shares_the_full_credential_floor() {
    use std::os::unix::fs::symlink;

    const FIXTURE: &str = "AFT_TEST_SANDBOX_LEVEL_FIXTURE";
    let Some(base) = std::env::var_os(FIXTURE).map(PathBuf::from) else {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let material = base.join("material");
        let mut kube_paths = vec![material.join("kube-one"), "../material/kube-two".into()];
        if cfg!(target_os = "linux") {
            kube_paths.push("kube/inside".into());
        }
        let test = concat!(
            module_path!(),
            "::sandbox_level_policy_selects_real_roles_and_shares_the_full_credential_floor"
        );
        // Only the new test process gets a different daemon HOME or tool env.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test.split_once("::").unwrap().1])
            .env(FIXTURE, &base)
            .env("HOME", base.join("daemon-alias"))
            .env("GNUPGHOME", material.join("gnupg"))
            .env("GH_CONFIG_DIR", material.join("gh"))
            .env("CLOUDSDK_CONFIG", material.join("cloudsdk"))
            .env("AZURE_CONFIG_DIR", material.join("azure"))
            .env("AWS_CONFIG_FILE", material.join("aws-config"))
            .env("AWS_SHARED_CREDENTIALS_FILE", material.join("aws-alias"))
            .env("DOCKER_CONFIG", material.join("docker"))
            .env("XDG_CONFIG_HOME", material.join("xdg"))
            .env("CARGO_HOME", material.join("cargo-alias"))
            .env("KUBECONFIG", std::env::join_paths(kube_paths).unwrap())
            .env("SSH_AUTH_SOCK", material.join("agent"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };

    let material = base.join("material");
    let home = base.join("daemon");
    let home_alias = base.join("daemon-alias");
    let account = base.join("account");
    for path in [&home, &account, &material] {
        std::fs::create_dir(path).unwrap();
    }
    symlink(&home, &home_alias).unwrap();
    std::fs::create_dir(material.join("gcloud-real")).unwrap();
    std::fs::create_dir(material.join("cargo")).unwrap();
    symlink(material.join("cargo"), material.join("cargo-alias")).unwrap();
    for name in ["aws", "kube-two", "cargo/Credentials-example"] {
        std::fs::write(material.join(name), "disposable fixture").unwrap();
    }
    symlink(material.join("aws"), material.join("aws-alias")).unwrap();
    std::fs::create_dir(account.join(".config")).unwrap();
    symlink(material.join("gcloud-real"), account.join(".config/gcloud")).unwrap();
    // Resolve a missing default from an existing symlinked ancestor.
    std::fs::create_dir(home.join(".kube")).unwrap();
    symlink(&material, home.join(".kube/other")).unwrap();
    symlink(home.join(".kube/other/future"), home.join(".kube/config")).unwrap();

    let (ctx, project, io, _fixture) = unix_fixture(&base);
    let checkout_kube = project.join("kube/inside");
    if cfg!(target_os = "linux") {
        std::fs::create_dir(project.join("kube")).unwrap();
        std::fs::write(&checkout_kube, "disposable kube fixture").unwrap();
    }
    #[cfg(target_os = "linux")]
    let _inventory = fixed_read_inventory::install(&base);
    ACCOUNT_HOME_OVERRIDE.with(|value| value.replace(Some(Ok(account.clone()))));

    let mut floor = BTreeSet::new();
    for h in [&home, &home_alias, &account] {
        floor.extend(
            [
                ".ssh",
                ".gnupg",
                "Library/Keychains",
                ".aws",
                ".config/gcloud",
                ".azure",
                ".config/gh",
                ".netrc",
                ".git-credentials",
                ".config/git/credentials",
                ".docker/config.json",
                ".npmrc",
                ".pypirc",
                ".kube/config",
            ]
            .map(|path| h.join(path)),
        );
    }
    floor.extend(
        [
            "gnupg",
            "gh",
            "cloudsdk",
            "azure",
            "aws-config",
            "aws",
            "aws-alias",
            "docker/config.json",
            "xdg/gh",
            "xdg/gcloud",
            "xdg/git/credentials",
            "kube-one",
            "kube-two",
            "cargo/Credentials-example",
            "cargo-alias/Credentials-example",
            "future",
            "gcloud-real",
        ]
        .map(|path| material.join(path)),
    );
    if cfg!(target_os = "linux") {
        floor.insert(checkout_kube.clone());
    }
    let prefixes = BTreeSet::from([
        home.join(".cargo/credentials"),
        home_alias.join(".cargo/credentials"),
        account.join(".cargo/credentials"),
        material.join("cargo/credentials"),
        material.join("cargo-alias/credentials"),
    ]);
    let mut credential_denies = floor.clone();
    credential_denies.insert(base.join("private-notes"));

    for worker in [false, true] {
        with_worker_jail(worker, || {
            for mode in [
                SandboxMode::Off,
                SandboxMode::Credentials,
                SandboxMode::Standard,
                SandboxMode::Strict,
            ] {
                set_role_modes(&ctx, mode, worker);
                let canary_reads = usize::from(
                    mode == SandboxMode::Strict
                        || mode == SandboxMode::Standard && (!worker || !cfg!(target_os = "linux")),
                );
                let plan = crate::worker_jail_canary::with_test_preparation_home(
                    &account,
                    canary_reads,
                    || resolve(&ctx, &AuthenticatedPrincipal::FirstParty, &io),
                );
                if mode == SandboxMode::Off && !worker {
                    assert_eq!(plan, SpawnPlan::Unsandboxed);
                    continue;
                }
                let SpawnPlan::Launcher { profile, .. } = &plan else {
                    panic!("{mode} worker={worker}: {plan:?}")
                };
                assert_eq!(profile.mode, mode);
                let read: BTreeSet<_> = profile.read_deny.iter().cloned().collect();
                let write: BTreeSet<_> = profile.write_deny.iter().cloned().collect();
                if mode == SandboxMode::Off {
                    assert!(read.is_empty() && write.is_empty());
                    assert!(profile.linux_mounts.is_none());
                } else {
                    assert_eq!(profile.worker_jail, worker);
                    assert_eq!(
                        profile
                            .deny_prefixes
                            .iter()
                            .cloned()
                            .collect::<BTreeSet<_>>(),
                        prefixes
                    );
                    let mut fallback_write = write.clone();
                    if cfg!(target_os = "linux") {
                        let mounts = profile
                            .linux_mounts
                            .as_ref()
                            .expect("local Linux mount policy");
                        let mut expected_masks = credential_denies.clone();
                        expected_masks.insert(material.join("agent"));
                        assert_eq!(
                            mounts
                                .credential_masks
                                .iter()
                                .cloned()
                                .collect::<BTreeSet<_>>(),
                            expected_masks
                        );
                        assert!(mounts.cargo_homes.contains(&material.join("cargo-alias")));
                        if mode == SandboxMode::Strict {
                            assert_eq!(
                                mounts
                                    .fallback_write_deny
                                    .iter()
                                    .cloned()
                                    .collect::<BTreeSet<_>>(),
                                floor
                            );
                            assert!(!write.contains(&checkout_kube));
                            assert!(mounts.read_only.contains(&project.join(".git/hooks")));
                            assert!(!mounts.read_only.contains(&project.join(".git")));
                            fallback_write.extend(mounts.fallback_write_deny.iter().cloned());
                        }
                    }
                    for deny in &floor {
                        assert!(
                            read.contains(deny) && fallback_write.contains(deny),
                            "{mode} worker={worker} missing {}",
                            deny.display()
                        );
                    }
                }
                if matches!(mode, SandboxMode::Off | SandboxMode::Credentials) {
                    assert_eq!(
                        profile.worker_gh_fence,
                        worker.then_some(crate::sandbox_profile::WorkerGhFence::Unenforced)
                    );
                }
                if mode == SandboxMode::Credentials {
                    assert_eq!(read, credential_denies);
                    assert_eq!(write, credential_denies);
                    assert!(profile.data_policy.deny.is_empty());
                    assert!(profile.write_deny_nested.is_empty());
                    assert!(!plan.enforces_shell_containment());
                    assert!(plan
                        .worker_read_broker_unavailable()
                        .unwrap()
                        .contains("does not protect the shared task-temp root"));
                }
                plan.cleanup_unspawned();
            }
        });
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sandbox_level_policy_linux_standard_defers_only_checkout_covering_broad_binds() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path();
    let work = home.join("Work");
    std::fs::create_dir(&work).unwrap();
    let (ctx, project, io, _fixture) = unix_fixture(&work);
    let sibling = home.join("sibling-checkout");
    let hooks = project.join(".git/hooks");
    let mut profile = SandboxProfile::build(
        vec![project.clone()],
        vec![hooks.clone()],
        vec![work.clone(), sibling.clone()],
        vec![],
        vec![],
        vec![],
        vec![],
        io,
    )
    .unwrap();
    profile.mode = SandboxMode::Standard;
    linux_mount_policy::permissive(&mut profile, &ctx, home).unwrap();
    let mounts = profile.linux_mounts.unwrap();
    assert_eq!(mounts.deferred_read_only, [work]);
    assert_eq!(mounts.read_only, [hooks, sibling]);
}
