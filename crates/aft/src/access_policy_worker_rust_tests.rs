//! The built-in Rust library read grants a live worker scope gets, checked
//! through `AccessPolicy::install_scope_grants_from`, the one place every
//! scope policy receives them. Each test builds its own Cargo and rustup homes
//! in a temporary directory and its own `WorkerRustSources`, so nothing here
//! reads or changes the test process's environment.
//!
//! Every refusal test also checks that a granted path is admitted by the same
//! policy. Without that, a refusal would still pass if the built-in grants
//! were never installed at all.

use super::*;

const SOURCE: &str = "pub fn rust_library_marker() {}\n";

const REGISTRY_FILE: &str = "registry/src/example/lib.rs";
const GIT_CHECKOUT_FILE: &str = "git/checkouts/example-1a2b3c/abcdef0/src/lib.rs";
const TOOLCHAIN_FILE: &str = "toolchains/stable/lib/rustlib/src/rust/library/core/src/lib.rs";

struct RustHome {
    _dir: tempfile::TempDir,
    base: PathBuf,
    cargo: PathBuf,
    rustup: PathBuf,
    sources: WorkerRustSources,
}

fn rust_home() -> RustHome {
    let dir = tempfile::tempdir().unwrap();
    let base = policy_path(dir.path());
    let home = base.join("home");
    let cargo = home.join(".cargo");
    let rustup = home.join(".rustup");
    for path in [
        cargo.join(REGISTRY_FILE),
        cargo.join(GIT_CHECKOUT_FILE),
        cargo.join("git/db/example-1a2b3c/config"),
        cargo.join("credentials.toml"),
        cargo.join("config.toml"),
        cargo.join("registry/cache/example.crate"),
        cargo.join("registry/index/config.json"),
        rustup.join(TOOLCHAIN_FILE),
        rustup.join("settings.toml"),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, SOURCE).unwrap();
    }
    let sources = WorkerRustSources::new(move |_variable, default| Some(home.join(default)));
    RustHome {
        _dir: dir,
        base,
        cargo,
        rustup,
        sources,
    }
}

impl RustHome {
    /// A fresh policy, as each tool call or consent check builds one.
    fn policy(&self, worker: bool) -> AccessPolicy {
        let mut policy = AccessPolicy::from_config(
            &Config::default(),
            &self.base.join("store"),
            vec![self.base.join("project")],
            &self.base.join("home"),
        );
        policy.install_scope_grants_from(Vec::new(), worker, &self.sources);
        policy
    }
}

fn read(policy: &AccessPolicy, path: &Path) -> AccessDecision {
    policy.decide(AccessOperation::Read, &policy_path(path), false)
}

fn write(policy: &AccessPolicy, path: &Path) -> AccessDecision {
    policy.decide(AccessOperation::Write, &policy_path(path), false)
}

#[test]
fn worker_rust_sources_admit_worker_reads_without_ask() {
    let home = rust_home();
    let worker = home.policy(true);
    for path in [
        home.cargo.join(REGISTRY_FILE),
        home.cargo.join(GIT_CHECKOUT_FILE),
        home.rustup.join(TOOLCHAIN_FILE),
    ] {
        assert_eq!(
            read(&worker, &path),
            AccessDecision::Allow,
            "{}",
            path.display()
        );
    }
    // A scope that is not a live worker gets no built-in grants.
    assert_eq!(
        read(&home.policy(false), &home.cargo.join(REGISTRY_FILE)),
        AccessDecision::Ask
    );
}

#[test]
fn worker_rust_sources_keep_stored_grants() {
    let home = rust_home();
    let shared = home.base.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    let stored = AccessGrant {
        path: policy_path(&shared),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Write,
    };
    let mut policy = home.policy(false);
    policy.install_scope_grants_from(vec![stored.clone()], true, &home.sources);
    assert_eq!(policy.grants.first(), Some(&stored));
    assert_eq!(write(&policy, &shared.join("a.rs")), AccessDecision::Allow);
    assert_eq!(
        read(&policy, &home.cargo.join(REGISTRY_FILE)),
        AccessDecision::Allow
    );
}

#[test]
fn worker_rust_sources_are_ordinary_policy_not_saved_scope_grants() {
    let home = rust_home();
    let shared = home.base.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    let _guard = scope_grants_guard(Some(ScopeGrantsDenial::NotSessionHost));
    let mut policy = home.policy(true);
    policy.install_scope_grants_from(
        vec![AccessGrant {
            path: shared.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        }],
        true,
        &home.sources,
    );
    assert!(policy.grants.is_empty());
    assert_eq!(
        read(&policy, &home.cargo.join(REGISTRY_FILE)),
        AccessDecision::Allow
    );
    assert_eq!(read(&policy, &shared), AccessDecision::Ask);
}

#[test]
fn worker_rust_sources_refuse_writes() {
    let home = rust_home();
    let worker = home.policy(true);
    let registry = home.cargo.join(REGISTRY_FILE);
    assert_eq!(read(&worker, &registry), AccessDecision::Allow);
    for path in [
        registry,
        home.cargo.join(GIT_CHECKOUT_FILE),
        home.rustup.join(TOOLCHAIN_FILE),
    ] {
        assert_eq!(
            write(&worker, &path),
            AccessDecision::Ask,
            "{}",
            path.display()
        );
    }
}

#[test]
fn worker_rust_sources_leave_git_db_and_credentials_out() {
    let home = rust_home();
    let worker = home.policy(true);
    assert_eq!(
        read(&worker, &home.cargo.join(REGISTRY_FILE)),
        AccessDecision::Allow
    );
    for path in [
        home.cargo.join("git/db/example-1a2b3c/config"),
        home.cargo.join("credentials.toml"),
        home.cargo.join("config.toml"),
        home.cargo.join("registry/cache/example.crate"),
        home.cargo.join("registry/index/config.json"),
        home.rustup.join("settings.toml"),
    ] {
        assert_eq!(
            read(&worker, &path),
            AccessDecision::Ask,
            "{}",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn worker_rust_sources_refuse_symlinks_pointing_out() {
    // The granted subtree itself replaced by a link to another project.
    let home = rust_home();
    let elsewhere = home.base.join("elsewhere-project");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("secret.rs"), "secret_marker").unwrap();
    let registry = home.cargo.join("registry/src");
    std::fs::rename(&registry, home.cargo.join("registry/src-parked")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &registry).unwrap();
    let worker = home.policy(true);
    assert_eq!(
        read(&worker, &home.cargo.join(GIT_CHECKOUT_FILE)),
        AccessDecision::Allow
    );
    assert_eq!(
        read(&worker, &elsewhere.join("secret.rs")),
        AccessDecision::Ask,
        "a registry/src symlink must not grant its target: {:?}",
        worker.grants
    );

    // A link on a component above the granted subtree.
    let home = rust_home();
    let outside = home.base.join("outside-registry");
    std::fs::create_dir_all(outside.join("src")).unwrap();
    std::fs::write(outside.join("src/secret.rs"), "secret_marker").unwrap();
    let registry = home.cargo.join("registry");
    std::fs::rename(&registry, home.cargo.join("registry-parked")).unwrap();
    std::os::unix::fs::symlink(&outside, &registry).unwrap();
    let worker = home.policy(true);
    assert_eq!(
        read(&worker, &home.rustup.join(TOOLCHAIN_FILE)),
        AccessDecision::Allow
    );
    assert_eq!(
        read(&worker, &outside.join("src/secret.rs")),
        AccessDecision::Ask,
        "a registry symlink must not grant its target: {:?}",
        worker.grants
    );
}

#[test]
fn worker_rust_sources_admit_directory_created_after_first_check() {
    // A fresh machine has no registry/src until the first crate download.
    let home = rust_home();
    let registry = home.cargo.join("registry/src");
    let parked = home.cargo.join("registry/src-parked");
    std::fs::rename(&registry, &parked).unwrap();
    let before = home.policy(true);
    assert_eq!(
        read(&before, &home.rustup.join(TOOLCHAIN_FILE)),
        AccessDecision::Allow
    );
    assert_eq!(
        read(&before, &home.cargo.join(REGISTRY_FILE)),
        AccessDecision::Ask
    );
    std::fs::rename(&parked, &registry).unwrap();
    let after = home.policy(true);
    assert_eq!(
        read(&after, &home.cargo.join(REGISTRY_FILE)),
        AccessDecision::Allow
    );
}
