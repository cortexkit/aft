//! The fixed deny list for `bash.env` and per-call `env` names.
//!
//! Two strengths, both case-insensitive at config load:
//!
//! - refused at every tier: the names can hijack process startup, redirect
//!   where tools (and AFT's own nested processes) find configuration and
//!   storage, inject code into an interpreter, corrupt or redirect Git, or
//!   drive AFT, the sandbox, or a build worker;
//! - refused at the project tier only: user configuration may set them, but a
//!   repository must not swap the user's credentials, identity, or the trust
//!   chain of its network traffic.
//!
//! The list is fixed in code and changes only through review; a config-time
//! extension could only ever tighten it, which nobody has asked for.

use std::collections::HashMap;

/// Names refused at every tier, matched case-insensitively.
pub(crate) const BLOCKED_ENV_VARS: &[&str] = &[
    // Loader injection (existing list).
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    // Shell startup.
    "BASH_ENV",
    "ENV",
    "IFS",
    "PATH",
    "SHELLOPTS",
    "BASHOPTS",
    "PROMPT_COMMAND",
    "PS4",
    // Interpreter startup.
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PYTHONHOME",
    "PERL5OPT",
    "PERL5LIB",
    "PERLLIB",
    "PERL5DB",
    "RUBYOPT",
    "RUBYLIB",
    "NODE_OPTIONS",
    "GCONV_PATH",
    "LOCPATH",
    "NLSPATH",
    "HOSTALIASES",
    "RESOLV_HOST_CONF",
    // Lookup roots: where tools and AFT's own nested processes find config and
    // storage. The sandbox overwrites the temp names and pins the cargo names,
    // so a configured value would apply unsandboxed and silently vanish
    // sandboxed.
    "HOME",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "COMSPEC",
    "PATHEXT",
    "SYSTEMROOT",
    "PSMODULEPATH",
    // Further interpreter injectors.
    "NODE_PATH",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "PYTHONUSERBASE",
    // Git: AFT appends its hooks path through GIT_CONFIG_COUNT/KEY/VALUE, so a
    // configured count corrupts it; the rest redirect the repository or run a
    // program. Harmless knobs (GIT_TERMINAL_PROMPT, GIT_PAGER, GIT_AUTHOR_*)
    // stay allowed.
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_EXEC_PATH",
    "GIT_TEMPLATE_DIR",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_EXTERNAL_DIFF",
    "GIT_PROXY_COMMAND",
    // AFT and module control: AFT carries its own controls in the same map.
    // The runner request already strips these prefixes.
    // (prefixes: AFT_, SUBC_, CORTEXKIT_, CORTEX_, CK_)
    // Sandbox and build-slot control: strict workers clear the compile-server
    // wrappers but relaxed mode only fills them when absent, the worker
    // test-thread budget keeps any caller value, and workers drop the session
    // bus.
    "RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUST_TEST_THREADS",
    "NEXTEST_TEST_THREADS",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Prefixes refused at every tier, matched case-insensitively.
pub(crate) const BLOCKED_ENV_PREFIXES: &[&str] = &[
    "LD_",
    "DYLD_",
    "BASH_FUNC_",
    "GIT_CONFIG",
    "AFT_",
    "SUBC_",
    "CORTEXKIT_",
    "CORTEX_",
    "CK_",
    "SCCACHE_",
];

/// The subset of [`BLOCKED_ENV_PREFIXES`] that belongs to AFT and its module
/// transport. User configuration must not set these, but a per-call `env` may:
/// the request is the agent's explicit choice for one command, and the product
/// strips these names from the child environment anyway, so refusing them only
/// breaks callers that pass them on purpose.
const MODULE_CONTROL_PREFIXES: &[&str] = &["AFT_", "SUBC_", "CORTEXKIT_", "CORTEX_", "CK_"];

/// Names refused at the project tier only. User configuration is the user's own
/// file and may set them.
pub(crate) const PROJECT_ONLY_BLOCKED_ENV_VARS: &[&str] = &[
    // Credentials and identity: a repository must not swap the user's identity
    // or point tools at its own endpoints.
    "SSH_AUTH_SOCK",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "KUBECONFIG",
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    // Network and TLS trust: a user sets these for a corporate proxy; a
    // repository must not.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "GIT_SSL_NO_VERIFY",
    "NODE_TLS_REJECT_UNAUTHORIZED",
];

/// Substrings refused at the project tier only, mirroring
/// `exec_remote::request::denied_environment_name` so one credential vocabulary
/// governs both the remote filter and project values.
pub(crate) const PROJECT_ONLY_BLOCKED_ENV_SUBSTRINGS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PRIVATE_KEY",
    "API_KEY",
    "CREDENTIAL",
];

/// Prefixes refused at the project tier only (credentials and identity).
pub(crate) const PROJECT_ONLY_BLOCKED_ENV_PREFIXES: &[&str] = &["AWS_", "GH_", "GITHUB_"];

/// Which tier a `bash.env` map came from. The strength of the deny list and
/// the disclosure wording both depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BashEnvTier {
    User,
    Project,
}

impl BashEnvTier {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

fn matches_name(key: &str, name: &str) -> bool {
    key.eq_ignore_ascii_case(name)
}

fn starts_with_prefix(key: &str, prefix: &str) -> bool {
    // Compare bytes rather than slicing at a byte index: a configured name may
    // be non-ASCII, and a cut inside a multi-byte character would panic.
    let key = key.as_bytes();
    let prefix = prefix.as_bytes();
    key.len() >= prefix.len() && key[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Whether `key` is refused at every tier.
pub(crate) fn is_blocked_env_key(key: &str) -> bool {
    BLOCKED_ENV_VARS
        .iter()
        .any(|blocked| matches_name(key, blocked))
        || BLOCKED_ENV_PREFIXES
            .iter()
            .any(|prefix| starts_with_prefix(key, prefix))
        || is_preexec_hijack_env_key(key)
}

/// Whether `key` may not be set from project configuration. Names refused at
/// every tier are also refused here: a single reason is reported per key.
pub(crate) fn is_project_blocked_env_key(key: &str) -> bool {
    if is_blocked_env_key(key) {
        return true;
    }
    PROJECT_ONLY_BLOCKED_ENV_VARS
        .iter()
        .any(|blocked| matches_name(key, blocked))
        || PROJECT_ONLY_BLOCKED_ENV_PREFIXES
            .iter()
            .any(|prefix| starts_with_prefix(key, prefix))
        || {
            let upper = key.to_ascii_uppercase();
            PROJECT_ONLY_BLOCKED_ENV_SUBSTRINGS
                .iter()
                .any(|part| upper.contains(part))
        }
}

/// The first name in `env` that `tier` may not set, if any.
pub(crate) fn first_refused_env_key(
    env: &std::collections::BTreeMap<String, String>,
    tier: BashEnvTier,
) -> Option<&str> {
    env.keys()
        .find(|key| match tier {
            BashEnvTier::User => is_blocked_env_key(key),
            BashEnvTier::Project => is_project_blocked_env_key(key),
        })
        .map(String::as_str)
}

/// Whether `key` is refused on a per-call `env` map. This is the config rule
/// minus the module-control prefixes: those names never reach a child (AFT
/// strips them), and a call that passes one explicitly is asking for it.
pub(crate) fn is_blocked_request_env_key(key: &str) -> bool {
    if MODULE_CONTROL_PREFIXES
        .iter()
        .any(|prefix| starts_with_prefix(key, prefix))
    {
        return BLOCKED_ENV_VARS
            .iter()
            .any(|blocked| matches_name(key, blocked));
    }
    is_blocked_env_key(key)
}

pub(crate) fn first_blocked_env_key(env: &HashMap<String, String>) -> Option<&str> {
    env.keys()
        .find(|key| is_blocked_request_env_key(key))
        .map(String::as_str)
}

/// Startup hooks that execute code before a command can enter its sandbox.
/// These names must be rejected for user-configured and per-call environments.
pub(crate) fn is_preexec_hijack_env_key(key: &str) -> bool {
    if starts_with_prefix(key, "LD_")
        || starts_with_prefix(key, "DYLD_")
        || starts_with_prefix(key, "BASH_FUNC_")
    {
        return true;
    }
    matches!(
        key,
        // The match is case-sensitive because every name here is uppercase;
        // fold first so `pythonpath` cannot slip past on Unix, where the name
        // still reaches an interpreter that reads it.
        _ if [
            "BASH_ENV",
            "ENV",
            "SHELLOPTS",
            "BASHOPTS",
            "PROMPT_COMMAND",
            "PS4",
            "IFS",
            "PYTHONSTARTUP",
            "PYTHONPATH",
            "PYTHONHOME",
            "PERL5OPT",
            "PERL5LIB",
            "PERLLIB",
            "PERL5DB",
            "RUBYOPT",
            "RUBYLIB",
            "NODE_OPTIONS",
            "GCONV_PATH",
            "LOCPATH",
            "NLSPATH",
            "HOSTALIASES",
            "RESOLV_HOST_CONF",
        ]
        .iter()
        .any(|name| matches_name(key, name))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_environment_rejects_both_blocklists() {
        for key in ["PATH", "LD_CUSTOM", "BASH_FUNC_echo%%", "NODE_OPTIONS"] {
            assert!(is_blocked_env_key(key), "{key}");
        }
        assert!(!is_blocked_env_key("TZ"));
    }

    #[test]
    fn blocked_names_fold_case_on_every_platform() {
        for key in [
            "path",
            "Path",
            "ld_preload",
            "aft_internal_linux_scope",
            "Git_Config_Count",
            "sccache_dir",
            "home",
        ] {
            assert!(is_blocked_env_key(key), "{key}");
        }
    }

    #[test]
    fn a_non_ascii_name_is_refused_instead_of_panicking() {
        // A byte-index prefix check panics when it cuts inside a multi-byte
        // character. Each name below starts with a refused prefix ("LD_",
        // "AFT_", "SCCACHE_") and continues with one, so the check must reach
        // a decision instead of slicing mid-character.
        assert!(
            is_blocked_env_key("LD_é"),
            "LD_-prefixed name must be refused"
        );
        assert!(
            is_blocked_env_key("AFT_é"),
            "AFT_-prefixed name must be refused"
        );
        assert!(is_blocked_env_key("SCCACHE_日本"));
        // A name sharing no refused prefix stays allowed.
        assert!(!is_blocked_env_key("é"));
        assert!(!is_blocked_env_key("IBToolNeverDequeé"));
        assert!(!is_project_blocked_env_key("é"));
    }

    #[test]
    fn module_control_names_are_refused_in_config_and_allowed_per_call() {
        // Configuration must never set these, but a call may pass one: AFT
        // strips them from the child anyway, and refusing them only breaks a
        // caller that asks for them explicitly.
        for key in ["AFT_INTERNAL_LINUX_SCOPE", "SUBC_MODULE_ID", "CORTEX_X"] {
            let request_env = std::collections::HashMap::from([(key.to_string(), "1".to_string())]);
            assert!(
                first_blocked_env_key(&request_env).is_none(),
                "{key} must be allowed on a per-call env"
            );
            let config_env = std::collections::BTreeMap::from([(key.to_string(), "1".to_string())]);
            assert_eq!(
                first_refused_env_key(&config_env, BashEnvTier::User),
                Some(key),
                "{key} must still be refused in configuration"
            );
        }
        // Names outside the module prefixes keep the full per-call rule.
        for key in ["PATH", "GIT_CONFIG_COUNT", "NODE_OPTIONS", "LD_PRELOAD"] {
            let request_env = std::collections::HashMap::from([(key.to_string(), "1".to_string())]);
            assert_eq!(first_blocked_env_key(&request_env), Some(key), "{key}");
        }
    }

    #[test]
    fn request_environment_keeps_harmless_git_and_project_names() {
        for key in [
            "TZ",
            "IBToolNeverDeque",
            "GIT_TERMINAL_PROMPT",
            "GIT_PAGER",
            "GIT_AUTHOR_NAME",
        ] {
            assert!(!is_blocked_env_key(key), "{key}");
            assert!(!is_project_blocked_env_key(key), "{key}");
        }
    }

    #[test]
    fn credentials_and_proxy_settings_are_project_tier_only() {
        for key in [
            "API_KEY",
            "NPM_TOKEN",
            "GITHUB_TOKEN",
            "AWS_PROFILE",
            "GH_HOST",
            "SSH_AUTH_SOCK",
            "HTTPS_PROXY",
            "NO_PROXY",
            "NODE_EXTRA_CA_CERTS",
        ] {
            assert!(!is_blocked_env_key(key), "user tier must keep {key}");
            assert!(
                is_project_blocked_env_key(key),
                "project tier must refuse {key}"
            );
        }
    }
}
