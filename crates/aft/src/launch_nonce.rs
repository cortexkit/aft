//! The launch nonce: the secret the subc daemon gives AFT when it spawns it
//! as a supervised module (`aft --subc`), which AFT presents on its HELLO and
//! on the routes it opens as a consumer, so the daemon admits it as itself.
//!
//! The daemon hands the nonce over once through an inherited pipe, and while
//! modules move to the pipe it also keeps an environment copy. Any process of
//! the same user can read another process's initial environment, and every
//! child inherits the live one, so AFT reads the nonce exactly once, at the
//! top of `main` and before it spawns anything, through
//! [`subc_os::launch_nonce()`] (the one reader that takes the pipe and falls
//! back to the environment copy only when no pipe is named). It then keeps the
//! value here for the life of the process and removes both variables from its
//! own environment, so no child (bash, PTY, language server, formatter, git,
//! the `gh` shim, a login-shell PATH probe) can inherit either of them.
//!
//! A standalone AFT (no `--subc`) never reads the accessor: it has no nonce
//! and must not consume a descriptor it was not given.

use std::fmt;
use std::sync::{Mutex, OnceLock};

use subc_protocol::manifest::{build_provenance, LaunchNonceSource, ManifestProvenance};

/// The nonce AFT was launched with and where it was read from. `Debug` never
/// prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct ModuleLaunchNonce {
    value: String,
    source: LaunchNonceSource,
}

impl ModuleLaunchNonce {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn source(&self) -> &LaunchNonceSource {
        &self.source
    }
}

impl fmt::Debug for ModuleLaunchNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModuleLaunchNonce")
            .field(
                "value",
                &format_args!("<{} bytes redacted>", self.value.len()),
            )
            .field("source", &self.source)
            .finish()
    }
}

enum Capture {
    /// `capture_at_startup` has not run: standalone mode, or an in-process
    /// test that has not installed a nonce.
    NotCaptured,
    Captured(Option<ModuleLaunchNonce>),
    /// The daemon named a descriptor that could not be read. The accessor
    /// never falls back to the environment copy in that case, and neither
    /// does AFT: the module runs without identity and the daemon refuses it.
    Failed(String),
}

static CAPTURED: Mutex<Capture> = Mutex::new(Capture::NotCaptured);

fn slot() -> std::sync::MutexGuard<'static, Capture> {
    CAPTURED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read the launch nonce once and remove it from this process's environment.
///
/// Call only in `--subc` mode, first thing in `main`: before any thread
/// starts (removing environment variables is unsound once other threads may
/// read the environment) and before anything spawns a child (until the read,
/// the pipe's descriptor is inheritable, and until the removal, so are both
/// variables). Logging is not up yet, so the outcome is reported later by
/// [`log_capture_outcome`].
pub fn capture_at_startup() {
    let outcome = match subc_os::launch_nonce() {
        Ok(Some(nonce)) => Capture::Captured(Some(ModuleLaunchNonce {
            value: nonce.value().to_string(),
            source: LaunchNonceSource::from_wire_name(nonce.source().as_str()),
        })),
        Ok(None) => Capture::Captured(None),
        Err(error) => Capture::Failed(error.to_string()),
    };
    *slot() = outcome;
    // The value is cached above and every reader in AFT goes through this
    // module (the subc client SDK reads the same cached accessor), so nothing
    // needs the environment copy any more. The descriptor variable goes too:
    // a child that inherited it without the pipe would find some unrelated
    // descriptor at that number.
    std::env::remove_var(subc_os::launch_nonce::LAUNCH_NONCE_ENV);
    std::env::remove_var(subc_os::launch_nonce::LAUNCH_NONCE_FD_ENV);
}

/// Log where the nonce came from, once logging is initialized.
pub fn log_capture_outcome() {
    match &*slot() {
        Capture::NotCaptured => {}
        Capture::Captured(Some(nonce)) => {
            log::info!("subc launch nonce read from {}", nonce.source().wire_name())
        }
        Capture::Captured(None) => {
            log::info!("subc launch nonce absent: this module was not spawned by the daemon")
        }
        Capture::Failed(error) => {
            log::warn!("subc launch nonce unavailable, connecting without module identity: {error}")
        }
    }
}

/// The nonce captured at startup, if any.
pub fn current() -> Option<ModuleLaunchNonce> {
    match &*slot() {
        Capture::Captured(nonce) => nonce.clone(),
        Capture::NotCaptured | Capture::Failed(_) => None,
    }
}

/// The identity AFT presents when it opens a route as a consumer: its module
/// id and the captured nonce. Passed explicitly on every route open so the
/// subc client never has to find the identity on its own.
pub fn consumer_identity() -> Option<subc_client_rs::ConsumerIdentity> {
    let module_id = std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let nonce = current()?;
    Some(subc_client_rs::ConsumerIdentity {
        module_id,
        launch_nonce: nonce.value,
    })
}

#[derive(Default)]
struct BuildDeclaration(OnceLock<ManifestProvenance>);

impl BuildDeclaration {
    fn initialize(&self, revision: Option<&str>) -> bool {
        if self.0.get().is_some() {
            return false;
        }
        self.0.set(build_facts(revision)).is_ok()
    }

    fn declared(&self) -> &ManifestProvenance {
        self.0.get_or_init(|| build_facts(None))
    }
}

static BUILD: BuildDeclaration = BuildDeclaration(OnceLock::new());

/// Install the binary's compiled revision before any provenance is requested.
/// The library deliberately has no compiled Git revision: embedding it would
/// invalidate its compiler cache for every commit, even with identical source.
/// Returns false if facts were already declared, including an unknown revision;
/// startup cannot replace an identity that has already been reported.
pub fn initialize_binary_build_revision(revision: Option<&str>) -> bool {
    BUILD.initialize(revision)
}

/// Build facts are declared on every HELLO, even when no launch nonce was
/// supplied. The protocol helper reports the version of the wire crate actually
/// linked into AFT; the nonce source remains a separate runtime fact.
pub fn provenance() -> Option<ManifestProvenance> {
    let declared = BUILD.declared();
    Some(
        declared
            .clone()
            .with_launch_nonce_source(current().map(|nonce| nonce.source)),
    )
}

fn build_facts(revision: Option<&str>) -> ManifestProvenance {
    match build_provenance(revision, None, None) {
        Ok(declared) => declared,
        Err(error) => {
            // A malformed build revision must not prevent module registration.
            // The caller caches these build facts, so this warning occurs once.
            log::warn!("subc build revision refused; declaring provenance without a SHA: {error}");
            ManifestProvenance::new().with_wire_crate_version(Some(
                subc_protocol::SUBC_PROTOCOL_CRATE_VERSION.to_string(),
            ))
        }
    }
}

/// Restores the previously captured nonce when dropped.
#[doc(hidden)]
pub struct InstalledForTests {
    previous: Option<Capture>,
}

impl Drop for InstalledForTests {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            *slot() = previous;
        }
    }
}

/// Stand in for `capture_at_startup` in tests that drive the subc loop inside
/// the test process, where `main` never runs. An empty or blank value counts
/// as no nonce. The installed nonce reports the environment as its source,
/// because that is where such a test supplies it.
#[doc(hidden)]
pub fn install_for_tests(nonce: Option<&str>) -> InstalledForTests {
    let installed =
        Capture::Captured(nonce.filter(|value| !value.trim().is_empty()).map(|value| {
            ModuleLaunchNonce {
                value: value.to_string(),
                source: LaunchNonceSource::Env,
            }
        }));
    let previous = std::mem::replace(&mut *slot(), installed);
    InstalledForTests {
        previous: Some(previous),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The captured nonce is process-wide; tests that replace it run one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn debug_never_prints_the_nonce() {
        let nonce = ModuleLaunchNonce {
            value: "secret-launch-nonce".to_string(),
            source: LaunchNonceSource::Fd,
        };
        let printed = format!("{nonce:?}");
        assert!(!printed.contains("secret-launch-nonce"), "{printed}");
        assert!(printed.contains("19 bytes redacted"), "{printed}");
    }

    #[test]
    fn refused_build_revision_preserves_wire_version_without_panicking() {
        let declared = build_facts(Some("not-a-git-sha"))
            .with_launch_nonce_source(Some(LaunchNonceSource::Fd));
        assert_eq!(declared.build_git_sha, None);
        assert_eq!(declared.wire_crate_version.as_deref(), Some("0.33.5"));
        assert_eq!(declared.launch_nonce_source, Some(LaunchNonceSource::Fd));
    }

    #[test]
    fn provenance_reports_build_facts_with_and_without_a_nonce() {
        let _serial = serial();
        let _none = install_for_tests(None);
        let assert_build_facts = |declared: &ManifestProvenance| {
            assert_eq!(declared.wire_crate_version.as_deref(), Some("0.33.5"));
            assert_eq!(
                declared.build_git_sha, None,
                "the library has no binary startup revision"
            );
        };
        let declared = provenance().expect("provenance without a nonce");
        assert_build_facts(&declared);
        assert_eq!(declared.launch_nonce_source, None);
        {
            let _env = install_for_tests(Some("n"));
            let declared = provenance().expect("provenance with a nonce");
            assert_build_facts(&declared);
            assert_eq!(declared.launch_nonce_source, Some(LaunchNonceSource::Env));
        }
        assert_eq!(current(), None, "the guard restores the previous capture");
    }

    #[test]
    fn binary_build_revision_is_declared_once_without_changing_wire_facts() {
        let declaration = BuildDeclaration::default();
        let revision = "a".repeat(40);
        assert!(declaration.initialize(Some(&revision)));
        assert_eq!(
            declaration.declared().build_git_sha.as_deref(),
            Some(revision.as_str())
        );
        assert_eq!(
            declaration.declared().wire_crate_version.as_deref(),
            Some("0.33.5")
        );
        assert!(!declaration.initialize(Some(&"b".repeat(40))));
        assert_eq!(
            declaration.declared().build_git_sha.as_deref(),
            Some(revision.as_str())
        );
    }

    #[test]
    fn unknown_build_revision_cannot_be_upgraded_after_provenance_is_reported() {
        let declaration = BuildDeclaration::default();
        assert_eq!(declaration.declared().build_git_sha, None);
        assert!(!declaration.initialize(Some(&"a".repeat(40))));
        assert_eq!(declaration.declared().build_git_sha, None);
        assert_eq!(
            declaration.declared().wire_crate_version.as_deref(),
            Some("0.33.5")
        );
    }

    /// Language servers, formatters, git and the ONNX Runtime probe are
    /// spawned with a plain `Command` that inherits AFT's own environment,
    /// without the bash child-environment funnel. What keeps the nonce out of
    /// them is that the startup capture removes both variables from AFT's
    /// environment, which this checks with such a child.
    #[test]
    fn startup_capture_leaves_nothing_for_a_plainly_spawned_child_to_inherit() {
        let _serial = serial();
        let _restore = install_for_tests(None);
        std::env::remove_var(subc_os::launch_nonce::LAUNCH_NONCE_FD_ENV);
        std::env::set_var(subc_os::launch_nonce::LAUNCH_NONCE_ENV, "unit-test-nonce");

        capture_at_startup();

        assert_eq!(
            current().as_ref().map(ModuleLaunchNonce::value),
            Some("unit-test-nonce"),
            "the nonce is kept in memory"
        );
        #[cfg(unix)]
        let output = std::process::Command::new("sh")
            .args(["-c", "env"])
            .output()
            .expect("spawn sh");
        #[cfg(windows)]
        let output = std::process::Command::new("cmd")
            .args(["/C", "set"])
            .output()
            .expect("spawn cmd");
        let environment = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{environment}");
        assert!(
            !environment.contains("SUBC_LAUNCH_NONCE"),
            "a child inherited a launch nonce variable: {environment}"
        );
    }
}
