/**
 * The fixed deny list for `bash.env` names, shared by both host plugins so the
 * engine and the plugins never disagree about what a configuration may set.
 *
 * Two strengths, both case-insensitive on every platform: names refused at
 * every tier, and names refused at the project tier only (a user's own file
 * may set credentials and proxy settings; a repository may not).
 *
 * The Rust engine keeps the same list in `crates/aft/src/bash_env.rs`; a
 * cross-language test feeds every Rust entry through these checks.
 */

/** Names refused at every tier. */
export const BLOCKED_BASH_ENV_NAMES: readonly string[] = [
  // Loader injection.
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
  // Lookup roots.
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
  // Git: redirection and program execution (AFT writes GIT_CONFIG_* itself).
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
  // Sandbox and build-slot control.
  "RUSTC_WRAPPER",
  "CARGO_BUILD_RUSTC_WRAPPER",
  "RUSTC_WORKSPACE_WRAPPER",
  "RUST_TEST_THREADS",
  "NEXTEST_TEST_THREADS",
  "DBUS_SESSION_BUS_ADDRESS",
];

/** Prefixes refused at every tier. */
export const BLOCKED_BASH_ENV_PREFIXES: readonly string[] = [
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

/** Names refused at the project tier only (credentials and identity). */
export const PROJECT_ONLY_BLOCKED_BASH_ENV_NAMES: readonly string[] = [
  "SSH_AUTH_SOCK",
  "GOOGLE_APPLICATION_CREDENTIALS",
  "KUBECONFIG",
  "DOCKER_HOST",
  "DOCKER_CONFIG",
];

/** Prefixes refused at the project tier only (credentials and identity). */
export const PROJECT_ONLY_BLOCKED_BASH_ENV_PREFIXES: readonly string[] = ["AWS_", "GH_", "GITHUB_"];

/** Substrings refused at the project tier only, mirroring the remote filter. */
export const PROJECT_ONLY_BLOCKED_BASH_ENV_SUBSTRINGS: readonly string[] = [
  "TOKEN",
  "SECRET",
  "PASSWORD",
  "PASSWD",
  "PRIVATE_KEY",
  "API_KEY",
  "CREDENTIAL",
];

/** Names refused at the project tier only (network and TLS trust). */
export const PROJECT_ONLY_NETWORK_BLOCKED_BASH_ENV_NAMES: readonly string[] = [
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

const BLOCKED = new Set(BLOCKED_BASH_ENV_NAMES);
const PROJECT_ONLY = new Set([
  ...PROJECT_ONLY_BLOCKED_BASH_ENV_NAMES,
  ...PROJECT_ONLY_NETWORK_BLOCKED_BASH_ENV_NAMES,
]);

/** Whether `name` is refused at every tier. */
export function isBlockedBashEnvKey(name: string): boolean {
  const upper = name.toUpperCase();
  return BLOCKED.has(upper) || BLOCKED_BASH_ENV_PREFIXES.some((prefix) => upper.startsWith(prefix));
}

/** Whether `name` may not be set from project configuration. */
export function isProjectBlockedBashEnvKey(name: string): boolean {
  const upper = name.toUpperCase();
  return (
    isBlockedBashEnvKey(name) ||
    PROJECT_ONLY.has(upper) ||
    PROJECT_ONLY_BLOCKED_BASH_ENV_PREFIXES.some((prefix) => upper.startsWith(prefix)) ||
    PROJECT_ONLY_BLOCKED_BASH_ENV_SUBSTRINGS.some((part) => upper.includes(part))
  );
}
