//! Minimal macOS Seatbelt binding for the sandbox launcher.
//!
//! This module contains the entire unsafe FFI surface. `apply()` constructs a
//! NUL-free profile, passes stable pointers to `sandbox_init`, and frees only
//! error buffers allocated by that API.

use aft::sandbox_profile::SandboxProfile;
#[cfg(target_os = "macos")]
use std::ffi::{CStr, CString};
#[cfg(target_os = "macos")]
use std::os::raw::c_char;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::ptr;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

#[cfg(target_os = "macos")]
pub(super) fn apply(profile: &SandboxProfile) -> Result<(), String> {
    let source = render_profile(profile)?;
    let source = CString::new(source)
        .map_err(|_| "generated Seatbelt profile contains a NUL byte".to_string())?;
    let mut error_buffer = ptr::null_mut();

    // SAFETY: `source` is a live NUL-terminated string, flags=0 selects raw
    // profile mode, and `error_buffer` is a valid out-pointer.
    let result = unsafe { sandbox_init(source.as_ptr(), 0, &mut error_buffer) };
    if result == 0 {
        return Ok(());
    }

    let message = if error_buffer.is_null() {
        format!("sandbox_init returned error code {result}")
    } else {
        // SAFETY: sandbox_init documents a NUL-terminated error string on
        // failure, owned by the sandbox API and released below.
        let message = unsafe { CStr::from_ptr(error_buffer) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: this non-null pointer came directly from sandbox_init.
        unsafe { sandbox_free_error(error_buffer) };
        message
    };
    Err(message)
}

fn render_profile(profile: &SandboxProfile) -> Result<String, String> {
    match profile.mode {
        aft::config::SandboxMode::Credentials => return render_credentials_profile(profile),
        aft::config::SandboxMode::Standard => return render_relaxed_profile(profile),
        _ => {}
    }
    let mut source = String::from(
        "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write* (literal \"/dev/null\"))\n",
    );
    render_signal_scope(&mut source, profile);
    render_secret_memory_boundary(&mut source, profile);

    for root in profile.write_allow_roots() {
        source.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            escape_path(root)?
        ));
    }
    for file in &profile.writable_files {
        source.push_str(&format!(
            "(allow file-write* (literal \"{}\"))\n",
            escape_path(file)?
        ));
    }
    for path in profile.write_deny.iter().chain(&profile.write_deny_nested) {
        source.push_str(&format!(
            "(deny file-write* (subpath \"{}\"))\n",
            escape_path(path)?
        ));
    }
    render_data_denies(
        &mut source,
        "file-write*",
        &profile.data_policy.deny,
        &profile.data_policy.write_allow,
        &profile.writable_files,
    )?;
    let reads = profile
        .data_policy
        .read_allow
        .iter()
        .chain(&profile.data_policy.read_files)
        .cloned()
        .collect::<Vec<_>>();
    let read_files = profile
        .writable_files
        .iter()
        .chain(&profile.data_policy.read_files)
        .cloned()
        .collect::<Vec<_>>();
    render_data_denies(
        &mut source,
        "file-read*",
        &profile.data_policy.deny,
        &reads,
        &read_files,
    )?;
    render_session_task_reads(&mut source, profile)?;
    // Git resolves the cwd by statting each ancestor. Permit metadata on the
    // exact exception's ancestor chain, not directory contents or sibling data.
    for allow in profile
        .data_policy
        .read_allow
        .iter()
        .chain(&profile.data_policy.read_files)
        .chain(&profile.data_policy.read_metadata_allow)
    {
        for ancestor in allow.ancestors() {
            if profile
                .data_policy
                .deny
                .iter()
                .any(|deny| ancestor.starts_with(deny))
                && !profile
                    .read_deny
                    .iter()
                    .any(|deny| ancestor.starts_with(deny))
            {
                source.push_str(&format!(
                    "(allow file-read-metadata (literal \"{}\"))\n",
                    escape_path(ancestor)?
                ));
            }
        }
    }
    for path in &profile.read_deny {
        source.push_str(&format!(
            "(deny file-read* ({} \"{}\"))\n",
            read_path_filter(path),
            escape_path(path)?
        ));
    }
    render_prefix_denies(&mut source, profile)?;
    render_ancestor_unlink_denies(&mut source, all_file_denies(profile))?;
    render_socket_denies(&mut source, profile)?;

    for port in &profile.loopback_tcp_connect_deny {
        source.push_str(&format!(
            "(deny network-outbound (remote tcp \"localhost:{port}\"))\n"
        ));
    }
    render_worker_canary_metadata(&mut source, profile)?;

    render_keychain_denies(&mut source, profile)?;
    Ok(source)
}

fn render_credentials_profile(profile: &SandboxProfile) -> Result<String, String> {
    let mut source = String::from("(version 1)\n(allow default)\n");
    render_signal_scope(&mut source, profile);
    for path in &profile.read_deny {
        source.push_str(&format!(
            "(deny file-read* file-write* {})\n",
            relaxed_filter(path, false)?
        ));
    }
    render_prefix_denies(&mut source, profile)?;
    render_ancestor_unlink_denies(
        &mut source,
        profile.read_deny.iter().chain(&profile.deny_prefixes),
    )?;
    render_socket_denies(&mut source, profile)?;
    render_keychain_item_denies(&mut source);
    Ok(source)
}

fn render_relaxed_profile(profile: &SandboxProfile) -> Result<String, String> {
    let mut source = String::from("(version 1)\n(allow default)\n");
    render_signal_scope(&mut source, profile);
    render_secret_memory_boundary(&mut source, profile);
    for path in &profile.write_deny {
        source.push_str(&format!(
            "(deny file-write* {})\n",
            relaxed_filter(path, false)?
        ));
    }
    render_ancestor_unlink_denies(&mut source, all_file_denies(profile))?;
    for path in &profile.read_deny {
        source.push_str(&format!(
            "(deny file-read* {})\n",
            relaxed_filter(path, false)?
        ));
    }
    render_prefix_denies(&mut source, profile)?;
    let writes = profile
        .write_allow_roots()
        .into_iter()
        .map(Path::to_path_buf)
        .chain(profile.writable_files.iter().cloned())
        .collect::<Vec<_>>();
    render_relaxed_scopes(
        &mut source,
        "file-write*",
        &profile.write_deny_nested,
        &writes,
        &profile.writable_files,
        &profile.writable_roots,
    )?;
    render_relaxed_scopes(
        &mut source,
        "file-write*",
        &profile.data_policy.deny,
        &profile.data_policy.write_allow,
        &profile.writable_files,
        &[],
    )?;
    let reads = profile
        .data_policy
        .read_allow
        .iter()
        .chain(&profile.data_policy.read_files)
        .cloned()
        .collect::<Vec<_>>();
    let read_files = profile
        .writable_files
        .iter()
        .chain(&profile.data_policy.read_files)
        .cloned()
        .collect::<Vec<_>>();
    render_relaxed_scopes(
        &mut source,
        "file-read*",
        &profile.data_policy.deny,
        &reads,
        &read_files,
        &[],
    )?;
    render_session_task_reads(&mut source, profile)?;
    for allow in profile
        .data_policy
        .read_allow
        .iter()
        .chain(&profile.data_policy.read_files)
        .chain(&profile.data_policy.read_metadata_allow)
    {
        for ancestor in allow.ancestors() {
            if profile
                .data_policy
                .deny
                .iter()
                .any(|deny| ancestor.starts_with(deny))
                && !profile
                    .read_deny
                    .iter()
                    .any(|deny| ancestor.starts_with(deny))
            {
                source.push_str(&format!(
                    "(allow file-read-metadata (literal \"{}\"))\n",
                    escape_path(ancestor)?
                ));
            }
        }
    }
    render_worker_canary_metadata(&mut source, profile)?;
    render_socket_denies(&mut source, profile)?;
    render_keychain_denies(&mut source, profile)?;
    Ok(source)
}

fn all_file_denies(profile: &SandboxProfile) -> impl Iterator<Item = &std::path::PathBuf> {
    profile
        .write_deny
        .iter()
        .chain(&profile.write_deny_nested)
        .chain(&profile.deny_prefixes)
        .chain(&profile.read_deny)
        .chain(&profile.data_policy.deny)
        .chain(&profile.keychain_deny)
}

fn render_ancestor_unlink_denies<'a>(
    source: &mut String,
    denies: impl Iterator<Item = &'a std::path::PathBuf>,
) -> Result<(), String> {
    // Path-based denies must survive renaming any enclosing directory, including
    // parents of private files outside the usual credential and store locations.
    let ancestors = denies
        .flat_map(|path| path.ancestors().skip(1))
        .collect::<std::collections::BTreeSet<_>>();
    for ancestor in ancestors {
        source.push_str(&format!(
            "(deny file-write-unlink (literal \"{}\"))\n",
            escape_path(ancestor)?
        ));
    }
    Ok(())
}

fn render_prefix_denies(source: &mut String, profile: &SandboxProfile) -> Result<(), String> {
    for path in &profile.deny_prefixes {
        source.push_str(&format!(
            "(deny file-read* file-write* {})\n",
            relaxed_filter(path, true)?
        ));
    }
    Ok(())
}

fn render_socket_denies(source: &mut String, profile: &SandboxProfile) -> Result<(), String> {
    for path in &profile.socket_deny {
        source.push_str(&format!(
            "(deny network-outbound (path \"{}\"))\n",
            escape_path(path)?
        ));
    }
    Ok(())
}

fn render_secret_memory_boundary(source: &mut String, profile: &SandboxProfile) {
    if profile.secret_proxy {
        source.push_str("(deny mach-priv-task-port)\n");
    }
}

#[cfg(test)]
#[test]
fn secret_bash_task_ports_are_denied_only_for_secret_profiles() {
    let root = tempfile::tempdir().unwrap();
    let mut profile = SandboxProfile::build(
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        root.path().to_owned(),
    )
    .unwrap();
    for mode in [
        aft::config::SandboxMode::Strict,
        aft::config::SandboxMode::Standard,
    ] {
        profile.mode = mode;
        profile.secret_proxy = false;
        let ordinary = render_profile(&profile).unwrap();
        assert!(!ordinary.contains("mach-priv-task-port"));
        profile.secret_proxy = true;
        let secret = render_profile(&profile).unwrap();
        assert!(secret.contains("(deny mach-priv-task-port)\n"));
        assert_eq!(secret.replace("(deny mach-priv-task-port)\n", ""), ordinary);
    }
}

fn render_signal_scope(source: &mut String, profile: &SandboxProfile) {
    if !profile.worker_jail {
        return;
    }
    // Separate sandbox_init calls do not share a signal domain, even with an
    // identical profile. Host-side AFT cancellation remains available across
    // commands; a command may signal only processes in its own sandbox.
    source.push_str("(deny signal)\n(allow signal (target same-sandbox))\n");
    // Signal 0 existence probes deliver no signal; scripts use their success
    // to judge liveness, including for processes outside this command.
    source.push_str("(allow signal (signal-number 0))\n");
}

const KEYCHAIN_ITEM_SERVICES: &[&str] = &[
    "com.apple.SecurityServer",           // File-based keychains (securityd).
    "com.apple.securityd.xpc",            // Data-protection keychain (secd).
    "com.apple.securityd.systemkeychain", // System-keychain service in recovery.
];

fn render_keychain_item_denies(source: &mut String) {
    // Deny item lookups without denying trust evaluation or code signing.
    for service in KEYCHAIN_ITEM_SERVICES {
        source.push_str(&format!("(deny mach-lookup (global-name \"{service}\"))\n"));
    }
}

fn render_keychain_denies(source: &mut String, profile: &SandboxProfile) -> Result<(), String> {
    render_keychain_item_denies(source);
    render_worker_broker_denies(source, profile)?;
    if !profile.worker_jail {
        return Ok(());
    }
    // Workers at Standard and Strict also retain their authorization-service denies.
    for service in [
        "com.apple.authd",                // Authorization Services rights and interaction.
        "com.apple.security.agent",       // Keychain unlock / ACL prompts.
        "com.apple.security.agent.login", // Privileged login-session agent.
        "com.apple.security.authhost",    // Authorization mechanism host.
        "com.apple.security.authtrampoline", // Authorization execution trampoline.
    ] {
        source.push_str(&format!("(deny mach-lookup (global-name \"{service}\"))\n"));
    }
    // Keep these last: neither writable project roots nor private-store read
    // exceptions may reopen a keychain directory. Match case aliases as well.
    for path in &profile.keychain_deny {
        source.push_str(&format!(
            "(deny file-read* file-write* {})\n",
            relaxed_filter(path, false)?
        ));
    }
    Ok(())
}

fn render_worker_broker_denies(
    source: &mut String,
    profile: &SandboxProfile,
) -> Result<(), String> {
    #[cfg(not(unix))]
    {
        let _ = (source, profile);
        return Ok(());
    }
    #[cfg(unix)]
    {
        if !profile.worker_jail {
            return Ok(());
        }
        // Broker endpoints are host-owned siblings of task temp directories.
        // A worker may connect to its own endpoint, never another command's.
        let name = profile
            .temp_dir
            .file_name()
            .ok_or("worker temp has no name")?
            .to_string_lossy();
        let own_socket = profile
            .temp_dir
            .with_file_name(format!("{name}.gh-read.sock"));
        let root = aft::sandbox_spawn::managed_task_temp_root()
            .to_str()
            .ok_or("broker root is not UTF-8")?;
        source.push_str(&format!(
        "(deny network-outbound (require-all (remote unix-socket (path-regex #\"^{}/[^/]+[.]gh-read[.]sock$\")) (require-not (path \"{}\"))))\n",
        regex::escape(root),
        escape_path(&own_socket)?
    ));
        Ok(())
    }
}

/// The shim stats and resolves its marker before a no-report data-access query.
/// Permit only metadata, after broader denies, so those lookups do not generate
/// sandbox violations. Data reads and writes remain denied by the worker profile.
fn render_worker_canary_metadata(
    source: &mut String,
    profile: &SandboxProfile,
) -> Result<(), String> {
    if let Some(aft::sandbox_profile::WorkerGhFence::Canary { paths }) = &profile.worker_gh_fence {
        for path in paths {
            for ancestor in path.ancestors() {
                source.push_str(&format!(
                    "(allow file-read-metadata (literal \"{}\"))\n",
                    escape_path(ancestor)?
                ));
            }
        }
    }
    Ok(())
}

fn render_relaxed_scopes(
    source: &mut String,
    operation: &str,
    denies: &[std::path::PathBuf],
    allows: &[std::path::PathBuf],
    files: &[std::path::PathBuf],
    equal_roots: &[std::path::PathBuf],
) -> Result<(), String> {
    for deny in denies {
        source.push_str(&format!(
            "(deny {operation} (require-all {}",
            relaxed_filter(deny, false)?
        ));
        for allow in allows
            .iter()
            // Only checkout exceptions may equal a denied directory, such as ~/Work.
            // Unrelated nested repos then belong to that checkout; linked siblings
            // retain their own denies. Private-store allows stay strict descendants.
            .filter(|allow| {
                allow.starts_with(deny) && (*allow != deny || equal_roots.contains(allow))
            })
        {
            let filter = if files.contains(allow) {
                format!("(literal \"{}\")", escape_path(allow)?)
            } else {
                relaxed_filter(allow, false)?
            };
            source.push_str(&format!(" (require-not {filter})"));
        }
        source.push_str("))\n");
    }
    Ok(())
}

/// Canonicalize before rendering, then also match ASCII case aliases for
/// missing filenames on case-insensitive volumes (for example .SSH).
fn relaxed_filter(path: &Path, prefix: bool) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| "Seatbelt path is not UTF-8".to_string())?;
    let mut pattern = String::from("^");
    for character in text.chars() {
        if character.is_control() {
            return Err(format!(
                "Seatbelt path contains a control character: {text:?}"
            ));
        } else if character == '"' {
            pattern.push_str("\\\"");
        } else if character.is_ascii_alphabetic() {
            pattern.push_str(&format!(
                "[{}{}]",
                character.to_ascii_lowercase(),
                character.to_ascii_uppercase()
            ));
        } else {
            pattern.push_str(&regex::escape(&character.to_string()));
        }
    }
    pattern.push_str(if prefix { "[^/]*($|/)" } else { "($|/)" });
    // SBPL #"..." retains regex backslashes. Ordinary string escaping would
    // turn \. into \\. and match a backslash instead of a literal dot.
    Ok(format!("(regex #\"{pattern}\")"))
}

fn session_task_pattern(root: &Path) -> Result<String, String> {
    use aft::bash_background::persistence::{readable_task_file_names, TaskLayout};
    let text = root.to_str().ok_or("Seatbelt session path is not UTF-8")?;
    if text.chars().any(char::is_control) {
        return Err("Seatbelt session path contains a control character".into());
    }
    let names = |layout| {
        readable_task_file_names(layout)
            .iter()
            .map(|name| regex::escape(name))
            .collect::<Vec<_>>()
            .join("|")
    };
    let task = format!("bash-{}", "[0-9a-f]".repeat(16));
    Ok(format!(
        "^{}/({task}/({})|{task}\\.({}))$",
        regex::escape(text),
        names(TaskLayout::Directory),
        names(TaskLayout::Flat)
    ))
}

fn render_session_task_reads(source: &mut String, profile: &SandboxProfile) -> Result<(), String> {
    let Some(root) = &profile.data_policy.session_task_read_dir else {
        return Ok(());
    };
    // This allow must follow the storage denies. Only registered readable names
    // match; resolved symlink targets outside the session do not match the regex.
    let pattern = session_task_pattern(root)?.replace('"', "\\\"");
    source.push_str(&format!(
        "(allow file-read* (require-all (regex #\"{pattern}\")"
    ));
    for path in &profile.read_deny {
        source.push_str(&format!(" (require-not {})", relaxed_filter(path, false)?));
    }
    for path in &profile.deny_prefixes {
        source.push_str(&format!(" (require-not {})", relaxed_filter(path, true)?));
    }
    source.push_str("))\n");
    for ancestor in root.ancestors() {
        source.push_str(&format!(
            "(allow file-read-metadata (literal \"{}\"))\n",
            escape_path(ancestor)?
        ));
    }
    Ok(())
}

fn render_data_denies(
    source: &mut String,
    operation: &str,
    denies: &[std::path::PathBuf],
    allows: &[std::path::PathBuf],
    exact_files: &[std::path::PathBuf],
) -> Result<(), String> {
    for deny in denies {
        // Exclude only strict descendants. An enclosing project or write_allow
        // must not turn the whole CortexKit store into an exception.
        source.push_str(&format!(
            "(deny {operation} (require-all (subpath \"{}\")",
            escape_path(deny)?
        ));
        for allow in allows
            .iter()
            .filter(|allow| *allow != deny && allow.starts_with(deny))
        {
            source.push_str(&format!(
                " (require-not ({} \"{}\"))",
                if exact_files.contains(allow) {
                    "literal"
                } else {
                    read_path_filter(allow)
                },
                escape_path(allow)?
            ));
        }
        source.push_str("))\n");
    }
    Ok(())
}

fn read_path_filter(path: &Path) -> &'static str {
    if path.is_file() {
        "literal"
    } else {
        // A missing deny target may become a directory after the profile is
        // installed, so treating it as a subpath keeps future children denied.
        "subpath"
    }
}

fn escape_path(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("Seatbelt path is not valid UTF-8: {}", path.display()))?;
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            character if character.is_control() => {
                return Err(format!(
                    "Seatbelt path contains a control character: {path:?}"
                ));
            }
            character => escaped.push(character),
        }
    }
    Ok(escaped)
}

#[cfg(all(test, target_os = "macos"))]
mod macos_native_tests {
    use super::*;
    use aft::config::{Config, SandboxMode};
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::process::{Command, Output};

    const LEVELS: [SandboxMode; 3] = [
        SandboxMode::Credentials,
        SandboxMode::Standard,
        SandboxMode::Strict,
    ];
    const SEED: &[u8] = b"disposable sandbox value";

    fn command(profile: Option<&Path>, executable: &str) -> Command {
        if let Some(path) = profile {
            let mut command = Command::new("/usr/bin/sandbox-exec");
            command.arg("-f").arg(path).arg(executable);
            command
        } else {
            Command::new(executable)
        }
    }

    fn assert_success(output: Output) -> Vec<u8> {
        assert!(output.status.success(), "{output:?}");
        output.stdout
    }

    fn file_oracle(profile: &Path, path: &Path) {
        let read = |profile| command(profile, "/bin/cat").arg(path).output().unwrap();
        let append = |profile| {
            command(profile, "/bin/sh")
                .args(["-c", "printf tamper >> \"$1\"", "append"])
                .arg(path)
                .output()
                .unwrap()
        };
        assert_eq!(assert_success(read(None)), SEED);
        assert_success(append(None));
        assert_eq!(fs::read(path).unwrap(), [SEED, b"tamper"].concat());
        fs::write(path, SEED).unwrap();

        let output = read(Some(profile));
        assert!(
            !output.status.success() && output.stdout.is_empty(),
            "{output:?}"
        );
        let output = append(Some(profile));
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(fs::read(path).unwrap(), SEED);
    }

    fn head_profile(root: &Path, level: SandboxMode) -> PathBuf {
        use aft::sandbox_spawn::{
            resolve_sandbox_spawn, AuthenticatedPrincipal, RequestedSandboxTier, SandboxTaskKind,
            SpawnPlan,
        };
        use aft_engine::test_bridge::bash_remote::cleanup_task_temp_dir;
        use aft_engine::test_bridge::bash_remote::sandbox_spawn::install_sandbox_launcher_for_test;

        let checkout = root.join("checkout");
        let storage = root.join("home/.local/share/cortexkit/aft");
        let ctx = aft::context::AppContext::new(
            Box::new(aft::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(checkout.clone()),
                storage_dir: Some(storage.clone()),
                ..Default::default()
            },
        );
        ctx.update_config(|config| {
            config.sandbox.profile.head = level;
            config.sandbox.profile.worker = SandboxMode::Off;
            config.sandbox.read_deny = vec![checkout.join("notes/private")];
            config.github.shim = false;
            config.git.co_author = "off".into();
        });
        let task = aft::bash_background::persistence::create_task_layout(
            &storage,
            "seatbelt-probe",
            &format!("bash-{:016x}", level as u8),
        )
        .unwrap();
        let _launcher =
            install_sandbox_launcher_for_test(&checkout, std::env::current_exe().unwrap());
        let plan = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            &task.paths.io_dir,
            None,
        );
        let SpawnPlan::Launcher { profile, .. } = plan else {
            panic!("{level}: {plan:?}")
        };
        let temp = profile.temp_dir.clone();
        let result = profile
            .canonicalize_for_launch()
            .map_err(|error| error.to_string())
            .and_then(|profile| render_profile(&profile));
        cleanup_task_temp_dir(&temp).unwrap();
        let rendered = root.join(format!("{level}.sb"));
        fs::write(&rendered, result.unwrap()).unwrap();
        rendered
    }

    #[test]
    fn files_and_agent_socket_follow_the_real_head_profile() {
        const FIXTURE: &str = "AFT_SEATBELT_NATIVE_FIXTURE";
        const CONNECT: &str = "AFT_SEATBELT_CONNECT_PATH";
        let name = concat!(
            module_path!(),
            "::files_and_agent_socket_follow_the_real_head_profile"
        );
        let test = name.split_once("::").unwrap().1;
        let exe = std::env::current_exe().unwrap();
        if let Some(path) = std::env::var_os(CONNECT) {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => println!("socket connected"),
                Err(error) if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM)) => {
                    println!("socket denied");
                }
                Err(error) => panic!("socket fixture failed: {error}"),
            }
            return;
        }
        let Some(base) = std::env::var_os(FIXTURE).map(PathBuf::from) else {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let output = Command::new(&exe)
                .args(["--exact", test, "--nocapture"])
                .env(FIXTURE, &root)
                .env("HOME", root.join("home"))
                .env("CARGO_HOME", root.join("checkout/cargo"))
                .env("AWS_SHARED_CREDENTIALS_FILE", root.join("aws-override"))
                .env("SSH_AUTH_SOCK", root.join("agent-alias"))
                .env(
                    "KUBECONFIG",
                    std::env::join_paths([
                        root.join("kube-absolute"),
                        PathBuf::from("kube-relative"),
                    ])
                    .unwrap(),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{output:?}"
            );
            return;
        };

        for dir in [
            "home/.kube",
            "checkout/cargo",
            "checkout/notes",
            "checkout/.git/hooks",
            "home/.local/share/cortexkit/aft",
        ] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        // Make .git look like a real repository to the engine's Git policy resolver.
        assert_success(
            Command::new("/usr/bin/git")
                .args(["init", "-q"])
                .arg(base.join("checkout"))
                .output()
                .unwrap(),
        );
        let denied = [
            "home/.netrc",
            "home/.kube/config",
            "kube-absolute",
            "checkout/kube-relative",
            "aws-override",
            "checkout/notes/private",
        ];
        for name in denied {
            fs::write(base.join(name), SEED).unwrap();
        }
        let hooks = base.join("checkout/.git/hooks/probe");
        fs::write(&hooks, SEED).unwrap();
        let cortexkit = base.join("home/.local/share/cortexkit/fixture");
        fs::write(&cortexkit, SEED).unwrap();

        let agent = base.join("agent");
        let alias = base.join("agent-alias");
        let per_call = base.join("per-call-agent");
        let _listeners = [
            UnixListener::bind(&agent).unwrap(),
            UnixListener::bind(&per_call).unwrap(),
        ];
        symlink(&agent, &alias).unwrap();
        let connect = |profile: Option<&Path>, socket: &Path, expected: &str| {
            let output = assert_success(
                command(profile, exe.to_str().unwrap())
                    .args(["--exact", test, "--nocapture"])
                    .env(CONNECT, socket)
                    .output()
                    .unwrap(),
            );
            let stdout = String::from_utf8_lossy(&output);
            assert!(stdout.contains(expected), "{socket:?}: {stdout}");
        };
        for socket in [&agent, &alias, &per_call] {
            connect(None, socket, "socket connected");
        }

        let mut strict = None;
        for level in LEVELS {
            let profile = head_profile(&base, level);
            for name in denied {
                file_oracle(&profile, &base.join(name));
            }
            for socket in [&agent, &alias] {
                connect(Some(&profile), socket, "socket denied");
            }
            connect(Some(&profile), &per_call, "socket connected");

            if level == SandboxMode::Credentials {
                assert_eq!(
                    assert_success(
                        command(Some(&profile), "/bin/cat")
                            .arg(&cortexkit)
                            .output()
                            .unwrap()
                    ),
                    SEED
                );
                let outside = base.join("outside-checkout");
                assert_success(
                    command(Some(&profile), "/bin/sh")
                        .args(["-c", "printf open > \"$1\"", "write"])
                        .arg(&outside)
                        .output()
                        .unwrap(),
                );
                assert_eq!(fs::read(outside).unwrap(), b"open");
            } else {
                let output = command(Some(&profile), "/bin/sh")
                    .args(["-c", "printf tamper >> \"$1\"", "append"])
                    .arg(&hooks)
                    .output()
                    .unwrap();
                assert!(!output.status.success(), "{level}: {output:?}");
                assert_eq!(fs::read(&hooks).unwrap(), SEED);
            }
            if level == SandboxMode::Strict {
                strict = Some(profile);
            }
        }
        strict_rename_and_future_prefix(&base, &strict.unwrap());
    }

    fn strict_rename_and_future_prefix(base: &Path, profile: &Path) {
        use std::io::Write;
        use std::process::Stdio;
        use std::time::{Duration, Instant};

        let notes = base.join("checkout/notes");
        let moved = notes.with_file_name("moved-notes");
        let private = notes.join("private");
        let before = fs::metadata(&private).unwrap();
        assert_success(
            command(None, "/bin/mv")
                .args([&notes, &moved])
                .output()
                .unwrap(),
        );
        assert_eq!(
            fs::metadata(moved.join("private")).unwrap().ino(),
            before.ino()
        );
        fs::rename(&moved, &notes).unwrap();
        let output = command(Some(profile), "/bin/mv")
            .args([&notes, &moved])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(!moved.exists());
        let after = fs::metadata(&private).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(fs::read(&private).unwrap(), SEED);

        let cargo = base.join("checkout/cargo");
        let future = cargo.join("credentials-after-start");
        let renamed = cargo.join("renamed");
        let sibling = cargo.join("config.toml");
        let ready = cargo.join("ready");
        assert!(!future.exists());
        let mut child = command(Some(profile), "/bin/sh")
            .args([
                "-c",
                r#"
                : > "$4" || exit 90
                read -r go || exit 91
                if /bin/cat "$1"; then exit 92; fi
                if (printf tamper >> "$1"); then exit 93; fi
                if /bin/mv "$1" "$2"; then exit 94; fi
                printf sibling > "$3"
            "#,
                "future-cargo-probe",
            ])
            .args([&future, &renamed, &sibling, &ready])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "Strict payload never started: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                Instant::now() < deadline,
                "Strict payload did not signal readiness"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!future.exists());
        fs::write(&future, SEED).unwrap();
        child.stdin.take().unwrap().write_all(b"ready\n").unwrap();
        let stdout = assert_success(child.wait_with_output().unwrap());
        assert!(stdout.is_empty(), "{stdout:?}");
        assert_eq!(fs::read(&future).unwrap(), SEED);
        assert!(!renamed.exists());
        assert_eq!(fs::read(&sibling).unwrap(), b"sibling");
    }

    fn isolated_profile(root: &Path, level: SandboxMode) -> PathBuf {
        let mut profile = SandboxProfile::build(
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            root.to_owned(),
        )
        .unwrap();
        profile.mode = level;
        let path = root.join(format!("{level}.sb"));
        fs::write(&path, render_profile(&profile).unwrap()).unwrap();
        path
    }

    #[test]
    fn file_keychain_query_fails_while_its_file_stays_readable() {
        struct Keychain(PathBuf);
        impl Drop for Keychain {
            fn drop(&mut self) {
                let _ = Command::new("/usr/bin/security")
                    .arg("delete-keychain")
                    .arg(&self.0)
                    .output();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let chain = Keychain(root.join("disposable.keychain-db"));
        let security = |args: &[&str]| {
            assert_success(
                command(None, "/usr/bin/security")
                    .args(args)
                    .arg(&chain.0)
                    .output()
                    .unwrap(),
            )
        };
        security(&["create-keychain", "-p", "disposable-fixture"]);
        security(&["unlock-keychain", "-p", "disposable-fixture"]);
        const ITEM: &str = "aft-file-keychain-disposable-value";
        security(&[
            "add-generic-password",
            "-A",
            "-s",
            "aft-test-service",
            "-a",
            "fixture",
            "-w",
            ITEM,
        ]);
        let query = |profile: Option<&Path>| {
            command(profile, "/usr/bin/security")
                .args([
                    "find-generic-password",
                    "-s",
                    "aft-test-service",
                    "-a",
                    "fixture",
                    "-w",
                ])
                .arg(&chain.0)
                .output()
                .unwrap()
        };
        assert_eq!(
            assert_success(query(None)),
            format!("{ITEM}\n").into_bytes()
        );
        for level in LEVELS {
            let profile = isolated_profile(&root, level);
            assert!(!assert_success(
                command(Some(&profile), "/bin/cat")
                    .arg(&chain.0)
                    .output()
                    .unwrap()
            )
            .is_empty());
            let output = query(Some(&profile));
            assert!(!output.status.success(), "{level}: {output:?}");
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains(ITEM),
                "{level}: {output:?}"
            );
        }
        assert_eq!(
            assert_success(query(None)),
            format!("{ITEM}\n").into_bytes()
        );
    }

    #[test]
    fn data_protection_query_fails_if_the_runner_can_seed_an_item() {
        use std::io::Write;
        use std::process::Stdio;

        // A standalone process exercises the SecItem API before and after
        // sandbox application, without reusing a cached service connection.
        const PROBE: &str = r#"
            #import <Foundation/Foundation.h>
            #import <Security/Security.h>
            int main(int argc, char **argv) { @autoreleasepool {
                if (argc != 3) return 2;
                NSMutableDictionary *query = [@{
                    (__bridge id)kSecClass: (__bridge id)kSecClassGenericPassword,
                    (__bridge id)kSecUseDataProtectionKeychain: @YES,
                    (__bridge id)kSecAttrService: [NSString stringWithUTF8String:argv[2]],
                    (__bridge id)kSecAttrAccount: @"disposable-fixture"
                } mutableCopy];
                OSStatus status;
                if (!strcmp(argv[1], "add")) {
                    query[(__bridge id)kSecValueData] = [@"aft-data-protection-probe" dataUsingEncoding:NSUTF8StringEncoding];
                    status = SecItemAdd((__bridge CFDictionaryRef)query, NULL);
                } else if (!strcmp(argv[1], "delete")) {
                    status = SecItemDelete((__bridge CFDictionaryRef)query);
                } else {
                    CFTypeRef result = NULL;
                    query[(__bridge id)kSecReturnData] = @YES;
                    status = SecItemCopyMatching((__bridge CFDictionaryRef)query, &result);
                    if (status == errSecSuccess) {
                        NSData *data = CFBridgingRelease(result);
                        fwrite(data.bytes, 1, data.length, stdout);
                    }
                }
                fprintf(stderr, "SecItem OSStatus=%d\n", (int)status);
                return status != errSecSuccess;
            } }
        "#;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("secitem-probe");
        let mut compiler = Command::new("/usr/bin/xcrun")
            .args([
                "clang",
                "-x",
                "objective-c",
                "-fobjc-arc",
                "-framework",
                "Foundation",
                "-framework",
                "Security",
                "-o",
            ])
            .arg(&exe)
            .arg("-")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        compiler
            .stdin
            .take()
            .unwrap()
            .write_all(PROBE.as_bytes())
            .unwrap();
        assert_success(compiler.wait_with_output().unwrap());

        struct Item {
            exe: PathBuf,
            service: String,
        }
        impl Drop for Item {
            fn drop(&mut self) {
                let _ = Command::new(&self.exe)
                    .args(["delete", &self.service])
                    .output();
            }
        }
        let service = format!(
            "aft-data-protection-{}-{}",
            std::process::id(),
            dir.path().file_name().unwrap().to_string_lossy()
        );
        let output = Command::new(&exe).args(["add", &service]).output().unwrap();
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            // A VM may lack a logged-in keychain, or an ad-hoc probe may not
            // have a keychain access group. Neither case can show whether the
            // sandbox prevents retrieval of an item an unrestricted process can read.
            if ["SecItem OSStatus=-25291\n", "SecItem OSStatus=-34018\n"].contains(&error.as_ref())
            {
                eprintln!(
                    "skipping data-protection item oracle: Off could not seed the fixture ({})",
                    error.trim()
                );
                return;
            }
            panic!("failed to seed the data-protection fixture: {output:?}");
        }
        let item = Item { exe, service };
        let query = |profile: Option<&Path>| {
            command(profile, item.exe.to_str().unwrap())
                .args(["find", &item.service])
                .output()
                .unwrap()
        };
        let expected = b"aft-data-protection-probe";
        assert_eq!(assert_success(query(None)), expected);
        for level in LEVELS {
            let profile = isolated_profile(dir.path(), level);
            let output = query(Some(&profile));
            assert!(
                !output.status.success() && output.stdout.is_empty(),
                "{level}: {output:?}"
            );
        }
        assert_eq!(assert_success(query(None)), expected);
    }

    #[test]
    fn system_trust_evaluator_verifies_a_system_root_offline() {
        let dir = tempfile::tempdir().unwrap();
        let certificate = dir.path().join("isrg-root-x1.pem");
        fs::write(
            &certificate,
            assert_success(
                command(None, "/usr/bin/security")
                    .args([
                        "find-certificate",
                        "-c",
                        "ISRG Root X1",
                        "-p",
                        "/System/Library/Keychains/SystemRootCertificates.keychain",
                    ])
                    .output()
                    .unwrap(),
            ),
        )
        .unwrap();
        let verify = |profile: Option<&Path>| {
            assert_success(
                command(profile, "/usr/bin/security")
            // -c supplies the certificate to verify, not a trusted anchor (-r).
            .args(["verify-cert", "-p", "basic", "-c"]).arg(&certificate).output().unwrap(),
            )
        };
        let off = verify(None);
        for level in LEVELS {
            let profile = isolated_profile(dir.path(), level);
            assert_eq!(verify(Some(&profile)), off);
        }
    }

    #[test]
    fn https_with_system_trust() {
        const URL_ENV: &str = "AFT_SEATBELT_HTTPS_URL";
        let Ok(url) = std::env::var(URL_ENV) else {
            eprintln!("skipping HTTPS system trust test: {URL_ENV} is not set (requires a network-enabled macOS runner)");
            return;
        };
        assert!(url.starts_with("https://"));
        let dir = tempfile::tempdir().unwrap();
        let fetch = |profile: Option<&Path>| {
            // nscurl uses NSURLSession with the platform's default certificate trust.
            let output = command(profile, "/usr/bin/nscurl")
                .args([
                    "--max-time",
                    "20",
                    "--ignore-location",
                    "-D",
                    "-",
                    "-o",
                    "/dev/null",
                    &url,
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                        line.starts_with("HTTP/") && line.split_whitespace().nth(1) == Some("200")
                    }),
                "{output:?}"
            );
        };
        fetch(None);
        for level in LEVELS {
            let profile = isolated_profile(dir.path(), level);
            fetch(Some(&profile));
        }
    }
}

#[cfg(test)]
mod tests {
    // Seatbelt rendering is macOS-only in production, and this fixture builds
    // the /var -> /private/var alias with a Unix symlink.
    #[cfg(unix)]
    #[test]
    fn scoped_var_folder_grants_render_canonical_write_and_read_allows() {
        use aft::access_policy::{AccessGrant, AccessOperation, AccessPolicy, GrantKind};
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let real = root.join("private/var/folders");
        let project = real.join("project");
        let data = real.join("private-data");
        let write = data.join("write");
        let read = data.join("read");
        let home = root.join("home");
        let temp = root.join("temp");
        for path in [&project, &write, &read, &home, &temp] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::os::unix::fs::symlink(root.join("private/var"), root.join("var")).unwrap();
        let alias = root.join("var/folders");
        let alias_write = alias.join("private-data/write");
        let alias_read = alias.join("private-data/read");
        let mut policy = AccessPolicy::from_config(
            &aft::config::Config::default(),
            &root.join("store"),
            vec![alias.join("project")],
            &home,
        );
        policy.grants = vec![
            AccessGrant {
                path: alias_write.clone(),
                operation: AccessOperation::Write,
                kind: GrantKind::Subtree,
            },
            AccessGrant {
                path: alias_read.clone(),
                operation: AccessOperation::Read,
                kind: GrantKind::Subtree,
            },
        ];
        let mut profile = policy.compile_shell(temp, Vec::new(), Vec::new()).unwrap();
        // Seatbelt reads are default-allowed. A data-store deny makes the held
        // read capability observable as a narrow exception to that deny.
        profile.data_policy.deny.push(alias.join("private-data"));
        let profile: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        let profile = profile.canonicalize_for_launch().unwrap();
        assert!(profile.writable_roots.contains(&write));
        assert!(profile.data_policy.read_allow.contains(&read));
        let source = render_profile(&profile).unwrap();
        let write = escape_path(&write).unwrap();
        let read = escape_path(&read).unwrap();
        let data = escape_path(&data).unwrap();
        assert!(
            source.contains(&format!("(allow file-write* (subpath \"{write}\"))")),
            "{source}"
        );
        assert!(source.contains(&format!(
            "(deny file-write* (require-all (subpath \"{data}\") (require-not (subpath \"{write}\"))))"
        )), "{source}");
        assert!(source.contains(&format!(
            "(deny file-read* (require-all (subpath \"{data}\") (require-not (subpath \"{read}\")) (require-not (subpath \"{write}\"))))"
        )), "{source}");
        for alias_path in [&alias_write, &alias_read] {
            assert!(
                !source.contains(&format!("\"{}\"", escape_path(alias_path).unwrap())),
                "{source}"
            );
        }
    }

    use super::*;

    #[test]
    fn signal_scope_is_limited_to_worker_profiles_at_every_enforcing_level() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::build(
            vec![dir.path().to_path_buf()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            dir.path().to_path_buf(),
        )
        .unwrap();
        for mode in [
            aft::config::SandboxMode::Credentials,
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            profile.worker_jail = false;
            let operator = render_profile(&profile).unwrap();
            assert!(!operator.contains("(deny signal)"));
            assert!(!operator.contains("(signal-number 0)"));
            profile.worker_jail = true;
            let source = render_profile(&profile).unwrap();
            assert!(source.contains("(deny signal)\n(allow signal (target same-sandbox))\n"));
            assert!(source.contains("(allow signal (signal-number 0))\n"));
            assert!(!source.contains("(target pgrp)"));
        }
    }

    #[test]
    fn keychain_mach_denies_cover_heads_and_workers_but_spare_trust() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::build(
            vec![dir.path().to_path_buf()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            dir.path().to_path_buf(),
        )
        .unwrap();
        for mode in [
            aft::config::SandboxMode::Credentials,
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            for worker in [false, true] {
                profile.worker_jail = worker;
                let source = render_profile(&profile).unwrap();
                let denies = |service| {
                    source.contains(&format!("(deny mach-lookup (global-name \"{service}\"))"))
                };
                assert!(denies("com.apple.SecurityServer"));
                assert!(denies("com.apple.securityd.xpc"));
                assert!(denies("com.apple.securityd.systemkeychain"));
                for service in [
                    "com.apple.security.agent",
                    "com.apple.security.agent.login",
                    "com.apple.authd",
                    "com.apple.security.authhost",
                    "com.apple.security.authtrampoline",
                ] {
                    assert_eq!(
                        denies(service),
                        worker && mode != aft::config::SandboxMode::Credentials
                    );
                }
                assert!(!source.contains("com.apple.trustd"));
                assert!(source.contains("(allow default)"));
            }
        }
    }

    #[test]
    fn worker_keychain_files_override_even_enclosing_grants() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::build(
            vec![dir.path().to_path_buf()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            dir.path().to_path_buf(),
        )
        .unwrap();
        profile.worker_jail = true;
        profile.keychain_deny = vec![
            dir.path().join("Library/Keychains"),
            Path::new("/Library/Keychains").to_path_buf(),
        ];
        for mode in [
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            let source = render_profile(&profile).unwrap();
            for path in &profile.keychain_deny {
                let deny = format!(
                    "(deny file-read* file-write* {})",
                    relaxed_filter(path, false).unwrap()
                );
                assert!(
                    source.contains(&deny),
                    "missing final keychain file deny: {source}"
                );
                assert!(source.find(&deny).unwrap() > source.rfind("(allow ").unwrap());
                for ancestor in path.ancestors().skip(1) {
                    let unlink_deny = format!(
                        "(deny file-write-unlink (literal \"{}\"))",
                        escape_path(ancestor).unwrap()
                    );
                    assert!(
                        source.contains(&unlink_deny),
                        "keychain ancestor can be renamed: {source}"
                    );
                }
            }
        }
    }

    #[test]
    fn worker_profiles_preserve_keychain_canary_and_gh_config_denies_together() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let canary_dir = home.join(aft::worker_jail_canary::CANARY_DIR_FROM_HOME);
        let canary = canary_dir.join(aft::worker_jail_canary::CANARY_FILE);
        let gh_config = home.join(".config/gh");
        let mut profile = SandboxProfile::build(
            vec![home.to_path_buf()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            home.to_path_buf(),
        )
        .unwrap();
        profile.worker_jail = true;
        profile.keychain_deny = vec![home.join("Library/Keychains"), "/Library/Keychains".into()];
        profile.read_deny = vec![canary_dir.clone(), gh_config.clone()];
        profile.write_deny = profile.read_deny.clone();
        profile.worker_gh_fence = Some(aft::sandbox_profile::WorkerGhFence::Canary {
            paths: vec![canary.clone()],
        });
        for mode in [
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            let source = render_profile(&profile).unwrap();
            for service in ["com.apple.SecurityServer", "com.apple.securityd.xpc"] {
                assert!(source.contains(&format!("(deny mach-lookup (global-name \"{service}\"))")));
            }
            assert!(!source.contains("com.apple.trustd"));
            let metadata = format!(
                "(allow file-read-metadata (literal \"{}\"))",
                escape_path(&canary).unwrap()
            );
            let metadata_index = source.find(&metadata).expect("canary must remain visible");
            for path in [&canary_dir, &gh_config] {
                let filter = if mode == aft::config::SandboxMode::Strict {
                    format!("(subpath \"{}\")", escape_path(path).unwrap())
                } else {
                    relaxed_filter(path, false).unwrap()
                };
                for operation in ["file-read*", "file-write*"] {
                    let deny = format!("(deny {operation} {filter})");
                    assert!(source
                        .find(&deny)
                        .is_some_and(|index| index < metadata_index));
                }
            }
            for path in &profile.keychain_deny {
                let deny = format!(
                    "(deny file-read* file-write* {})",
                    relaxed_filter(path, false).unwrap()
                );
                assert!(source
                    .find(&deny)
                    .is_some_and(|index| index > metadata_index));
            }
            assert!(!source.contains("(allow file-read-data"));
        }
    }

    #[test]
    fn relaxed_regex_literal_escapes_dot_once() {
        assert_eq!(
            relaxed_filter(Path::new("/tmp/a.b"), false).unwrap(),
            r##"(regex #"^/[tT][mM][pP]/[aA]\.[bB]($|/)")"##
        );
        assert_eq!(
            relaxed_filter(Path::new("/tmp/a.b"), true).unwrap(),
            r##"(regex #"^/[tT][mM][pP]/[aA]\.[bB][^/]*($|/)")"##
        );
        assert!(relaxed_filter(Path::new("/tmp/a\nb"), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn optional_exact_file_carve_outs_are_literal_even_when_not_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("common");
        std::fs::create_dir(&common).unwrap();
        let files =
            ["packed-refs", "packed-refs.lock", "packed-refs.new"].map(|name| common.join(name));
        let target = dir.path().join("config");
        std::fs::write(&target, "protected").unwrap();
        std::fs::create_dir(&files[1]).unwrap();
        std::os::unix::fs::symlink(&target, &files[2]).unwrap();
        for operation in ["file-read*", "file-write*"] {
            let mut source = String::new();
            render_data_denies(&mut source, operation, &[common.clone()], &files, &files).unwrap();
            let exceptions = files
                .iter()
                .map(|file| {
                    format!(
                        " (require-not (literal \"{}\"))",
                        escape_path(file).unwrap()
                    )
                })
                .collect::<String>();
            assert_eq!(
                source,
                format!(
                    "(deny {operation} (require-all (subpath \"{}\"){exceptions}))\n",
                    escape_path(&common).unwrap(),
                )
            );
        }
    }

    #[test]
    fn write_denies_follow_write_allows() {
        let root = tempfile::tempdir().expect("temp root");
        let project = root.path().join("project");
        let nested_deny = project.join(".git");
        let task_temp = root.path().join("task");
        std::fs::create_dir_all(&nested_deny).expect("nested deny directory");
        std::fs::create_dir_all(&task_temp).expect("task temp directory");
        let profile = SandboxProfile {
            secret_proxy: false,
            v: 1,
            mode: aft::config::SandboxMode::Strict,
            worker_jail: false,
            keychain_deny: Vec::new(),
            deny_prefixes: Vec::new(),
            data_policy: Default::default(),
            writable_roots: vec![project],
            writable_files: Vec::new(),
            write_deny: Vec::new(),
            write_deny_nested: vec![nested_deny],
            read_allow: Vec::new(),
            read_allow_listed: Vec::new(),
            read_deny: Vec::new(),
            socket_deny: Vec::new(),
            loopback_tcp_connect_deny: Vec::new(),
            cache_roots: Vec::new(),
            temp_dir: task_temp,
            worker_gh_fence: None,
        };
        let source = render_profile(&profile).expect("render profile");
        let allow = source.find("(allow file-write*").expect("write allow");
        let deny = source
            .rfind("(deny file-write*")
            .expect("nested write deny");
        assert!(deny > allow, "nested deny must be the last matching rule");
    }

    #[test]
    fn worker_canary_metadata_exceptions_follow_data_denies_for_every_home() {
        // Seatbelt profiles use macOS-style slash-separated paths. Construct them
        // without `Path::join` so Windows keeps the same profile text.
        let paths = ["/Users/account", "/Users/daemon"].map(|home| {
            std::path::PathBuf::from(format!(
                "{home}/{}/canary",
                aft::worker_jail_canary::CANARY_DIR_FROM_HOME
            ))
        });
        let dirs = paths
            .iter()
            .map(|path| path.parent().unwrap().to_path_buf())
            .collect::<Vec<_>>();
        let mut profile = SandboxProfile {
            secret_proxy: false,
            v: 1,
            mode: aft::config::SandboxMode::Strict,
            worker_jail: false,
            keychain_deny: Vec::new(),
            deny_prefixes: Vec::new(),
            data_policy: Default::default(),
            writable_roots: vec![Path::new("/work").to_path_buf()],
            writable_files: Vec::new(),
            write_deny: dirs.clone(),
            write_deny_nested: Vec::new(),
            read_allow: Vec::new(),
            read_allow_listed: Vec::new(),
            read_deny: dirs,
            socket_deny: Vec::new(),
            loopback_tcp_connect_deny: Vec::new(),
            cache_roots: Vec::new(),
            temp_dir: Path::new("/task").to_path_buf(),
            worker_gh_fence: Some(aft::sandbox_profile::WorkerGhFence::Canary {
                paths: paths.to_vec(),
            }),
        };
        for mode in [
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            let source = render_profile(&profile).unwrap();
            let last_deny = source.rfind("(deny file-read*").unwrap();
            assert!(source.contains("(deny file-write*"));
            for path in &paths {
                for ancestor in path.ancestors() {
                    let allow = format!(
                        "(allow file-read-metadata (literal \"{}\"))",
                        ancestor.display()
                    );
                    assert!(
                        source.find(&allow).is_some_and(|index| index > last_deny),
                        "missing metadata exception for {} in {mode}: {source}",
                        ancestor.display()
                    );
                }
            }
            assert!(!source.contains("(allow file-read-data"), "{source}");
        }
    }

    #[test]
    fn hooks_denies_follow_project_allow_for_git_friendly_ordering() {
        let root = tempfile::tempdir().expect("temp root");
        let project = root.path().join("project");
        let hooks = project.join(".git/hooks");
        let task_temp = root.path().join("task");
        std::fs::create_dir_all(&hooks).expect("hooks directory");
        std::fs::create_dir_all(&task_temp).expect("task temp directory");
        let profile = SandboxProfile::build(
            vec![project],
            vec![hooks.clone()],
            Vec::new(),
            Vec::new(),
            vec![hooks],
            Vec::new(),
            Vec::new(),
            task_temp,
        )
        .expect("build profile");

        let source = render_profile(&profile).expect("render profile");
        let allow = source
            .find("(allow file-write*")
            .expect("project write allow");
        let hook_write_deny = source
            .find("(deny file-write* (subpath")
            .expect("hooks write deny");
        let hook_read_deny = source
            .find("(deny file-read* (subpath")
            .expect("hooks read deny");
        assert!(hook_write_deny > allow);
        assert!(hook_read_deny > hook_write_deny);
    }

    #[test]
    fn secret_floor_denies_follow_enclosing_write_allow() {
        let root = tempfile::tempdir().expect("temp root");
        let home = root.path().join("home");
        let secret_floor = home.join(".ssh");
        let task_temp = home.join("task");
        std::fs::create_dir_all(&secret_floor).expect("secret floor directory");
        std::fs::create_dir_all(&task_temp).expect("task temp directory");
        let profile = SandboxProfile::build(
            vec![home],
            vec![secret_floor.clone()],
            Vec::new(),
            Vec::new(),
            vec![secret_floor],
            Vec::new(),
            Vec::new(),
            task_temp,
        )
        .expect("build profile");

        let source = render_profile(&profile).expect("render profile");
        let allow = source.find("(allow file-write*").expect("write allow");
        let write_deny = source
            .find("(deny file-write* (subpath")
            .expect("secret-floor write deny");
        let read_deny = source
            .find("(deny file-read* (subpath")
            .expect("secret-floor read deny");
        assert!(
            write_deny > allow,
            "secret-floor write deny must follow enclosing write allow"
        );
        assert!(
            read_deny > write_deny,
            "read deny must remain in the profile"
        );
    }

    #[test]
    fn missing_mandatory_denies_are_rendered_with_path_intent() {
        let root = tempfile::tempdir().expect("temp root");
        let project = root.path().join("project");
        let task_temp = root.path().join("task");
        std::fs::create_dir_all(&project).expect("project directory");
        std::fs::create_dir_all(&task_temp).expect("task temp directory");
        let missing_git = project.join(".git");
        let missing_secret = root.path().join("missing-secret");
        let missing_socket = root.path().join("missing.sock");
        let profile = SandboxProfile::build(
            vec![project],
            Vec::new(),
            vec![missing_git.clone()],
            Vec::new(),
            vec![missing_secret.clone()],
            vec![missing_socket.clone()],
            Vec::new(),
            task_temp,
        )
        .expect("build profile");

        let source = render_profile(&profile).expect("render profile");
        for rule in [
            format!(
                "(deny file-write* (subpath \"{}\"))",
                escape_path(&profile.write_deny_nested[0]).unwrap()
            ),
            format!(
                "(deny file-read* (subpath \"{}\"))",
                escape_path(&profile.read_deny[0]).unwrap()
            ),
            format!(
                "(deny network-outbound (path \"{}\"))",
                escape_path(&profile.socket_deny[0]).unwrap()
            ),
        ] {
            assert!(source.contains(&rule), "missing mandatory rule: {rule}");
        }
    }

    #[test]
    fn relaxed_nested_checkout_ancestors_are_immutable_without_denying_own_contents() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let work = root.join("Work");
        let own = work.join("own");
        let group = work.join("group");
        let sibling = group.join("sibling");
        let temp = root.join("temp");
        for dir in [&own, &sibling, &temp] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut profile = SandboxProfile::build(
            vec![own.clone()],
            Vec::new(),
            vec![work.clone(), sibling.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        profile.mode = aft::config::SandboxMode::Standard;
        let source = render_profile(&profile).unwrap();
        for ancestor in [&group, &work, &root] {
            let rule = format!(
                "(deny file-write-unlink (literal \"{}\"))",
                escape_path(ancestor).unwrap()
            );
            assert!(source.contains(&rule), "missing ancestor rule: {rule}");
        }
        assert!(!source.contains(&format!(
            "(deny file-write-unlink (literal \"{}\"))",
            escape_path(&own).unwrap()
        )));
        assert!(source.contains(&format!(
            "(deny file-write* (require-all {} (require-not {})))",
            relaxed_filter(&work, false).unwrap(),
            relaxed_filter(&own, false).unwrap()
        )));
    }

    #[test]
    fn relaxed_private_ancestors_and_equal_checkout_exceptions() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let work = root.join("Work");
        let sibling = work.join("sibling");
        let auth = root.join("relay/auth");
        let temp = root.join("temp");
        for dir in [&sibling, &auth, &temp] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let secret = auth.join("connection.json");
        std::fs::write(&secret, "private").unwrap();
        let mut profile = SandboxProfile::build(
            vec![work.clone()],
            Vec::new(),
            vec![work.clone(), sibling.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        profile.mode = aft::config::SandboxMode::Standard;
        profile.data_policy.deny.push(secret.clone());
        profile.data_policy.read_allow.push(secret.clone());
        profile.data_policy.write_allow.push(secret.clone());
        let source = render_profile(&profile).unwrap();
        assert!(
            source.contains(&format!(
                "(deny file-write-unlink (literal \"{}\"))",
                escape_path(&auth).unwrap()
            )),
            "private file ancestor must be immutable: {source}"
        );
        assert!(
            source.contains(&format!(
                "(deny file-write* (require-all {} (require-not {})))",
                relaxed_filter(&work, false).unwrap(),
                relaxed_filter(&work, false).unwrap()
            )),
            "equal checkout must be writable: {source}"
        );
        assert!(
            source.contains(&format!(
                "(deny file-write* (require-all {}))",
                relaxed_filter(&sibling, false).unwrap()
            )),
            "linked sibling must remain denied: {source}"
        );
        for operation in ["file-read*", "file-write*"] {
            assert!(
                source.contains(&format!(
                    "(deny {operation} (require-all {}))",
                    relaxed_filter(&secret, false).unwrap()
                )),
                "equal private-file grants must not weaken denies: {source}"
            );
        }
    }

    #[test]
    fn relaxed_granted_folder_carves_work_and_sibling_checkout_denies() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let work = home.join("Work");
        let own = work.join("own");
        let sibling = work.join("sibling");
        let granted = sibling.join("granted");
        let temp = home.join("temp");
        let config = granted.join(".git/config");
        for dir in [&own, &granted, &temp] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut profile = SandboxProfile::build(
            vec![own.clone(), granted.clone()],
            vec![config.clone()],
            vec![work.clone(), sibling.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        profile.mode = aft::config::SandboxMode::Standard;
        let source = render_profile(&profile).unwrap();
        assert!(
            source.contains(&format!(
                "(deny file-write* (require-all {} (require-not {}) (require-not {})))",
                relaxed_filter(&work, false).unwrap(),
                relaxed_filter(&own, false).unwrap(),
                relaxed_filter(&granted, false).unwrap(),
            )),
            "{source}"
        );
        assert!(
            source.contains(&format!(
                "(deny file-write* (require-all {} (require-not {})))",
                relaxed_filter(&sibling, false).unwrap(),
                relaxed_filter(&granted, false).unwrap(),
            )),
            "{source}"
        );
        assert!(
            source.contains(&format!(
                "(deny file-write* {})",
                relaxed_filter(&config, false).unwrap(),
            )),
            "{source}"
        );
    }

    #[test]
    fn relaxed_renderer_is_default_allow_with_scoped_and_case_alias_denies() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let work = root.join("Work");
        let own = work.join("own");
        let temp = root.join("temp");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let config = own.join(".git/config");
        let mut profile = SandboxProfile::build(
            vec![own.clone()],
            vec![config.clone()],
            vec![work.clone()],
            Vec::new(),
            vec![root.join(".ssh")],
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        profile.mode = aft::config::SandboxMode::Standard;
        profile.deny_prefixes.push(root.join(".cargo/credentials"));
        let source = render_profile(&profile).unwrap();
        assert!(source.starts_with("(version 1)\n(allow default)\n"));
        assert!(!source.contains("(deny file-write*)"));
        assert!(source.contains(&format!(
            "(deny file-write* {})",
            relaxed_filter(&config, false).unwrap()
        )));
        assert!(source.contains(&format!(
            "(require-all {} (require-not {}))",
            relaxed_filter(&work, false).unwrap(),
            relaxed_filter(&own, false).unwrap()
        )));
        assert!(source.contains("[sS][sS][hH]"));
        assert!(source.contains("[^/]*($|/)"));
        assert!(source.contains("file-write-unlink"));
        assert!(!source.contains("network-outbound"));
        assert!(!source.contains("no-sandbox"));
    }
}

// Seatbelt profiles exist only on macOS, and the generated regexes assume its
// slash-separated canonical paths; Windows paths would never match them.
#[cfg(all(test, unix))]
mod session_read_tests {
    use super::*;
    use aft::bash_background::persistence::{
        create_task_layout, readable_task_file_names, TaskLayout,
    };

    #[test]
    fn session_read_regex_matches_only_file_tool_names_after_storage_deny() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage.with[regex]+chars");
        let task = create_task_layout(&storage, "session", "bash-1111111111111111").unwrap();
        let session = task.paths.session_dir.canonicalize().unwrap();
        let pattern = session_task_pattern(&session).unwrap();
        let matcher = regex::Regex::new(&pattern).unwrap();
        for layout in [TaskLayout::Directory, TaskLayout::Flat] {
            for name in readable_task_file_names(layout) {
                let path = match layout {
                    TaskLayout::Directory => session.join(&task.paths.task_id).join(name),
                    TaskLayout::Flat => session.join(format!("{}.{name}", task.paths.task_id)),
                };
                assert!(
                    matcher.is_match(path.to_str().unwrap()),
                    "{}",
                    path.display()
                );
                assert!(!matcher.is_match(&format!("{}/child", path.display())));
            }
        }
        for path in [
            &task.paths.environment,
            &storage.join("aft.db"),
            &task.paths.io_dir.join("unexpected.sqlite"),
        ] {
            assert!(!matcher.is_match(path.to_str().unwrap()));
        }
        let mut profile = SandboxProfile::build(
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            temp.path().to_path_buf(),
        )
        .unwrap();
        profile
            .data_policy
            .deny
            .push(storage.canonicalize().unwrap());
        profile.data_policy.session_task_read_dir = Some(session);
        for mode in [
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            let source = render_profile(&profile).unwrap();
            let deny = source.find("(deny file-read*").unwrap();
            let allow = source
                .find("(allow file-read* (require-all (regex")
                .unwrap();
            assert!(allow > deny, "{source}");
            assert!(
                source.contains(&format!("(regex #\"{pattern}\")")),
                "{source}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod broker_root_render_tests {
    use super::*;
    #[test]
    fn gh_shim_broker_seatbelt_root_uses_the_shared_namespace_definition() {
        let fixture = tempfile::tempdir().unwrap();
        let root = aft::sandbox_spawn::managed_task_temp_root();
        let own = root.join("0123456789ab");
        let mut profile = SandboxProfile::build(
            vec![fixture.path().to_owned()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            fixture.path().to_owned(),
        )
        .unwrap();
        profile.worker_jail = true;
        profile.temp_dir = own.clone();
        let source = render_profile(&profile).unwrap();
        let expected=format!("(deny network-outbound (require-all (remote unix-socket (path-regex #\"^{}/[^/]+[.]gh-read[.]sock$\")) (require-not (path \"{}\"))))",regex::escape(root.to_str().unwrap()),escape_path(&own.with_file_name("0123456789ab.gh-read.sock")).unwrap());
        assert!(source.lines().any(|line|line==expected),"rendered worker socket rule must match the allocator namespace and exact own endpoint: {source}");
        profile.worker_jail = false;
        assert!(
            !render_profile(&profile).unwrap().contains("gh-read[.]sock"),
            "head profiles must not receive the worker socket restriction"
        );
    }
}

#[cfg(test)]
mod data_ancestor_read_tests {
    use super::*;

    fn nested_profile(root: &Path) -> SandboxProfile {
        let data = root.join("private-data");
        let exception = data.join("alfonso/worktrees/hash-a/own");
        let seeds = data.join("alfonso/worktrees/hash-a/.rust-build-seeds");
        let credential_floor = data.join("credentials");
        std::fs::create_dir_all(&exception).unwrap();
        std::fs::create_dir_all(credential_floor.join("checkout")).unwrap();
        std::fs::create_dir_all(data.join("alias")).unwrap();
        std::fs::create_dir(&seeds).unwrap();
        std::fs::write(seeds.join("seed"), "build seed").unwrap();
        let mut profile = SandboxProfile::build(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![credential_floor.clone()],
            Vec::new(),
            Vec::new(),
            root.to_path_buf(),
        )
        .unwrap();
        profile.data_policy.deny = vec![data.clone()];
        profile.data_policy.read_allow =
            vec![exception, credential_floor.join("checkout"), seeds.clone()];
        profile.write_deny.push(seeds);
        profile.data_policy.read_metadata_allow = vec![data.join("alias")];
        profile
    }

    #[test]
    fn own_checkout_read_exceptions_grant_only_ancestor_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut profile = nested_profile(&root);
        let data = root.join("private-data");
        let intermediate = data.join("alfonso");
        let worktrees = intermediate.join("worktrees");
        let hash = worktrees.join("hash-a");
        let own = hash.join("own");
        for mode in [
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            let source = render_profile(&profile).unwrap();
            let grants = source
                .lines()
                .filter(|line| line.starts_with("(allow file-read-metadata file-read-data "))
                .collect::<Vec<_>>();
            assert!(profile.data_policy.read_allow.contains(&own));
            assert!(!profile.data_policy.read_allow.contains(&worktrees));
            assert!(
                grants.is_empty(),
                "ancestor listings must remain denied: {mode:?}"
            );
            for ancestor in [&data, &intermediate, &worktrees, &hash] {
                assert!(source.contains(&format!(
                    "(allow file-read-metadata (literal \"{}\"))",
                    escape_path(ancestor).unwrap()
                )));
                assert!(!source.contains(&format!(
                    "(require-not {})",
                    relaxed_filter(ancestor, false).unwrap()
                )));
                for operation in [
                    "file-read*",
                    "file-read-data",
                    "file-read-metadata file-read-data",
                ] {
                    assert!(!source.contains(&format!(
                        "(allow {operation} (literal \"{}\"))",
                        escape_path(ancestor).unwrap()
                    )));
                    assert!(
                        !source.contains(&format!(
                            "(allow {operation} (subpath \"{}\"))",
                            escape_path(ancestor).unwrap()
                        )),
                        "ancestor grant must not cover descendants: {mode:?}"
                    );
                }
            }
            let own_filter = if mode == aft::config::SandboxMode::Standard {
                relaxed_filter(&own, false).unwrap()
            } else {
                format!("(subpath \"{}\")", escape_path(&own).unwrap())
            };
            assert!(source.contains(&format!("(require-not {own_filter})")));
            assert!(source.contains(&format!(
                "(allow file-read-metadata (literal \"{}\"))",
                escape_path(&data.join("alias")).unwrap()
            )));
        }
    }

    #[test]
    fn exact_worker_ancestor_files_never_become_subtree_or_write_exceptions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut profile = nested_profile(&root);
        let container = root.join("private-data/alfonso/worktrees/hash-a");
        let replaced = container.join(".cargo/config.toml");
        let missing = container.join(".cargo/config");
        std::fs::create_dir_all(&replaced).unwrap();
        profile.data_policy.read_files = vec![replaced.clone(), missing.clone()];
        let transferred: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        profile = transferred.canonicalize_for_launch().unwrap();
        assert_eq!(profile.data_policy.read_files.len(), 2);
        for mode in [
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            let source = render_profile(&profile).unwrap();
            let reads = source
                .lines()
                .filter(|line| line.starts_with("(deny file-read* "))
                .collect::<Vec<_>>();
            let writes = source
                .lines()
                .find(|line| line.starts_with("(deny file-write* (require-all "))
                .unwrap();
            for path in [&replaced, &missing] {
                let literal = format!("(literal \"{}\")", escape_path(path).unwrap());
                assert!(
                    reads
                        .iter()
                        .any(|line| line.contains(&format!("(require-not {literal})"))),
                    "{mode:?}"
                );
                assert!(
                    !writes.contains(&literal),
                    "read file must not be a write exception"
                );
                assert!(!source.contains(&format!("(subpath \"{}\")", escape_path(path).unwrap())));
                assert!(!source.contains(&relaxed_filter(path, false).unwrap()));
                assert!(!profile.writable_files.contains(path));
            }
            let seeds = container.join(".rust-build-seeds");
            let directory_filter = if mode == aft::config::SandboxMode::Standard {
                relaxed_filter(&seeds, false).unwrap()
            } else {
                format!("(subpath \"{}\")", escape_path(&seeds).unwrap())
            };
            assert!(reads
                .iter()
                .any(|line| line.contains(&format!("(require-not {directory_filter})"))));
            assert!(!writes.contains(&directory_filter));
            assert!(source.contains(&format!("(deny file-write* {directory_filter})")));
            assert!(!profile.data_policy.read_files.contains(&seeds));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_own_checkout_reads_preserve_ancestor_listing_and_sibling_denials() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut profile = nested_profile(&root);
        let data = root.join("private-data");
        let intermediate = data.join("alfonso");
        let worktrees = intermediate.join("worktrees");
        let own = worktrees.join("hash-a/own");
        let allowed = own.join("allowed.txt");
        let sibling = worktrees.join("hash-a/sibling/secret.txt");
        let other_repo = worktrees.join("hash-b/other/secret.txt");
        let main = data.join("repositories/main");
        std::fs::create_dir_all(&main).unwrap();
        let git = |args: &[&std::ffi::OsStr]| {
            let output = std::process::Command::new("/usr/bin/git")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init".as_ref(), "-q".as_ref(), main.as_os_str()]);
        git(&[
            "-C".as_ref(),
            main.as_os_str(),
            "-c".as_ref(),
            "user.name=Sandbox Test".as_ref(),
            "-c".as_ref(),
            "user.email=sandbox@example.invalid".as_ref(),
            "commit".as_ref(),
            "--allow-empty".as_ref(),
            "-qm".as_ref(),
            "initial".as_ref(),
        ]);
        git(&[
            "-C".as_ref(),
            main.as_os_str(),
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            own.as_os_str(),
        ]);
        // The linked checkout's common Git directory can itself be inside the
        // private store. Its explicit read exception must survive the boundary.
        profile.data_policy.read_allow.push(main.join(".git"));
        let config = worktrees.join("hash-a/.cargo/config.toml");
        let target = worktrees.join("hash-a/configured-target");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(&config, format!("[build]\ntarget-dir = {target:?}\n")).unwrap();
        profile.data_policy.read_files.push(config.clone());
        profile.data_policy.read_allow.push(target.clone());
        profile.data_policy.write_allow.push(target.clone());
        profile.writable_roots.push(target.clone());
        let sibling_target = worktrees.join("hash-a/sibling/target/artifact");
        std::fs::create_dir_all(sibling_target.parent().unwrap()).unwrap();
        std::fs::write(&sibling_target, "sibling artifact").unwrap();
        let sibling_seeds = worktrees.join("hash-a/sibling/.rust-build-seeds/seed");
        std::fs::create_dir_all(sibling_seeds.parent().unwrap()).unwrap();
        std::fs::write(&sibling_seeds, "sibling seed").unwrap();
        for path in [&sibling, &other_repo] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "sibling private").unwrap();
        }
        std::fs::write(&allowed, "exception readable").unwrap();
        for mode in [
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Standard,
        ] {
            profile.mode = mode;
            let rendered = root.join("profile.sb");
            std::fs::write(&rendered, render_profile(&profile).unwrap()).unwrap();
            let output = std::process::Command::new("/usr/bin/sandbox-exec")
                .current_dir(&own)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["-f"])
                .arg(&rendered)
                .args([
                    "/bin/sh",
                    "-c",
                    r#"set -eu
for ancestor in "$1" "$2" "$3" "$3/hash-a"; do
    if /bin/ls "$ancestor" >/dev/null 2>&1; then
        echo 'ancestor directory unexpectedly listable' >&2
        exit 1
    fi
done
for sibling in "$5" "$6"; do
    if /bin/cat "$sibling" >/dev/null 2>&1; then
        echo 'sibling file unexpectedly readable' >&2
        exit 1
    fi
done
[ "$(/bin/cat "$4/allowed.txt")" = 'exception readable' ]
/bin/cat "$4/../.cargo/config.toml" >/dev/null
[ "$(/bin/cat "$4/../.rust-build-seeds/seed")" = 'build seed' ]
if /bin/cat "$4/../sibling/.rust-build-seeds/seed" >/dev/null 2>&1; then
    echo 'sibling seed unexpectedly readable' >&2
    exit 1
fi
if (printf tamper >> "$4/../.rust-build-seeds/seed") 2>/dev/null; then
    echo 'container seed unexpectedly writable' >&2
    exit 1
fi
if (printf tamper > "$4/../.rust-build-seeds/new-seed") 2>/dev/null; then
    echo 'container seed directory unexpectedly writable' >&2
    exit 1
fi
"$7" --version
printf build > "$4/../configured-target/artifact"
[ "$(/bin/cat "$4/../configured-target/artifact")" = build ]
if /bin/cat "$4/../sibling/target/artifact" >/dev/null 2>&1; then
    echo 'sibling target unexpectedly readable' >&2
    exit 1
fi
if (printf tamper >> "$4/../.cargo/config.toml") 2>/dev/null; then
    echo 'container config unexpectedly writable' >&2
    exit 1
fi
status=$(/usr/bin/git -C "$4" status --porcelain)
[ "$status" = '?? allowed.txt' ]
echo 'ancestor listing denials and read boundaries passed'
"#,
                    "ancestor-read-probe",
                ])
                .args([
                    &data,
                    &intermediate,
                    &worktrees,
                    &own,
                    &sibling,
                    &other_repo,
                ])
                .arg(env!("CARGO"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{mode:?}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.lines().any(|line| line.starts_with("cargo ")));
            assert!(stdout.ends_with("ancestor listing denials and read boundaries passed\n"));
        }
    }
}
