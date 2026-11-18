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
    if profile.mode == aft::config::SandboxMode::Relaxed {
        return render_relaxed_profile(profile);
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
    render_data_denies(
        &mut source,
        "file-read*",
        &profile.data_policy.deny,
        &profile.data_policy.read_allow,
        &profile.writable_files,
    )?;
    render_session_task_reads(&mut source, profile)?;
    // Git resolves the cwd by statting each ancestor. Permit metadata on the
    // exact exception's ancestor chain, not directory contents or sibling data.
    for allow in profile
        .data_policy
        .read_allow
        .iter()
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
    for path in &profile.socket_deny {
        source.push_str(&format!(
            "(deny network-outbound (path \"{}\"))\n",
            escape_path(path)?
        ));
    }

    for port in &profile.loopback_tcp_connect_deny {
        source.push_str(&format!(
            "(deny network-outbound (remote tcp \"localhost:{port}\"))\n"
        ));
    }
    render_worker_canary_metadata(&mut source, profile)?;

    render_worker_keychain_denies(&mut source, profile)?;
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
    // Path-based denies must survive renaming any enclosing directory, including
    // parents of private files outside the usual credential and store locations.
    let ancestors = profile
        .write_deny
        .iter()
        .chain(&profile.write_deny_nested)
        .chain(&profile.deny_prefixes)
        .chain(&profile.read_deny)
        .chain(&profile.data_policy.deny)
        .chain(&profile.keychain_deny)
        .flat_map(|path| path.ancestors().skip(1))
        .collect::<std::collections::BTreeSet<_>>();
    for ancestor in ancestors {
        source.push_str(&format!(
            "(deny file-write-unlink (literal \"{}\"))\n",
            escape_path(ancestor)?
        ));
    }
    for path in &profile.read_deny {
        source.push_str(&format!(
            "(deny file-read* {})\n",
            relaxed_filter(path, false)?
        ));
    }
    for path in &profile.deny_prefixes {
        source.push_str(&format!(
            "(deny file-read* file-write* {})\n",
            relaxed_filter(path, true)?
        ));
    }
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
    render_relaxed_scopes(
        &mut source,
        "file-read*",
        &profile.data_policy.deny,
        &profile.data_policy.read_allow,
        &profile.writable_files,
        &[],
    )?;
    render_session_task_reads(&mut source, profile)?;
    for allow in profile
        .data_policy
        .read_allow
        .iter()
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
    render_worker_keychain_denies(&mut source, profile)?;
    Ok(source)
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
        aft::config::SandboxMode::Relaxed,
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

fn render_worker_keychain_denies(
    source: &mut String,
    profile: &SandboxProfile,
) -> Result<(), String> {
    render_worker_broker_denies(source, profile)?;
    if !profile.worker_jail {
        return Ok(());
    }
    // These are Security.framework's keychain and authorization endpoints, not
    // a com.apple.security.* wildcard. Trust evaluation and code-signing services
    // remain governed by the profile's default allow policy.
    for service in [
        "com.apple.SecurityServer", // Legacy file-keychain API (securityd).
        "com.apple.securityd.xpc",  // SecItem API (secd).
        "com.apple.securityd.systemkeychain", // System-keychain API in recovery.
        "com.apple.authd",          // Authorization Services rights and interaction.
        "com.apple.security.agent", // Keychain unlock / ACL prompts.
        "com.apple.security.agent.login", // Privileged login-session agent.
        "com.apple.security.authhost", // Authorization mechanism host.
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
    fn signal_scope_is_limited_to_worker_profiles_in_both_modes() {
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
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Relaxed,
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
    fn worker_keychain_mach_denies_preserve_trust_services() {
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
            aft::config::SandboxMode::Strict,
            aft::config::SandboxMode::Relaxed,
        ] {
            profile.mode = mode;
            profile.worker_jail = false;
            let head = render_profile(&profile).unwrap();
            assert!(!head.contains("mach-lookup"));
            profile.worker_jail = true;
            let source = render_profile(&profile).unwrap();
            for service in [
                "com.apple.SecurityServer",
                "com.apple.securityd.xpc",
                "com.apple.securityd.systemkeychain",
                "com.apple.security.agent",
                "com.apple.security.agent.login",
                "com.apple.authd",
                "com.apple.security.authhost",
                "com.apple.security.authtrampoline",
            ] {
                assert!(
                    source.contains(&format!("(deny mach-lookup (global-name \"{service}\"))")),
                    "missing {service}: {source}"
                );
            }
            for service in ["com.apple.trustd", "com.apple.trustd.agent"] {
                assert!(
                    !source.contains(&format!("(global-name \"{service}\")")),
                    "trust service must remain default-allowed"
                );
            }
            assert!(source.contains("(allow default)"));
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
            aft::config::SandboxMode::Relaxed,
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
                if mode == aft::config::SandboxMode::Relaxed {
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
            aft::config::SandboxMode::Relaxed,
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
            aft::config::SandboxMode::Relaxed,
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
        profile.mode = aft::config::SandboxMode::Relaxed;
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
        profile.mode = aft::config::SandboxMode::Relaxed;
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
        profile.mode = aft::config::SandboxMode::Relaxed;
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
        profile.mode = aft::config::SandboxMode::Relaxed;
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
            aft::config::SandboxMode::Relaxed,
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
