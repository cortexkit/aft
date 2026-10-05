//! Whole-line routing. The frozen worker envelope is the only policy source.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub commands: Vec<String>,
}

/// Keep the entire line local whenever the literal bash grammar cannot prove
/// that every command is allowed. Prefixes compare words, not substrings.
pub fn matches(policy: &RemoteExecPolicy, line: &str, pty: bool, stdin: bool) -> bool {
    if !policy.enabled || pty || stdin || policy.commands.iter().any(|p| !valid_prefix(p)) {
        return false;
    }
    let Some(commands) = crate::bash_rewrite::parser::parse_top_level(line) else {
        return false;
    };
    !commands.is_empty()
        && commands.iter().all(|command| {
            !forbidden_executable(&command[0])
                && !command.iter().any(|word| {
                    matches!(
                        word.split('=').next().unwrap_or(""),
                        "--fix" | "--run-ignored" | "--ignored" | "--include-ignored"
                    )
                })
                && policy.commands.iter().any(|prefix| {
                    let words: Vec<_> = prefix.split_whitespace().collect();
                    command.len() >= words.len() && words.iter().zip(command).all(|(a, b)| *a == b)
                })
        })
}

fn forbidden_executable(executable: &str) -> bool {
    let basename = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    let basename = basename.strip_suffix(".exe").unwrap_or(&basename);
    matches!(basename, "git" | "gh" | "eval")
}

pub(super) fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && !forbidden_executable(prefix.split(' ').next().unwrap_or(""))
        && prefix.split(' ').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_./+-".contains(&b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn published_policy_vectors_match_and_verify_digests() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/exec_remote/fixtures/policy");
        let mut count = 0;
        for entry in std::fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let canonical = std::fs::read(path.with_extension("jcs")).unwrap();
            assert_eq!(serde_json::to_vec(&value).unwrap(), canonical, "{path:?}");
            assert_eq!(
                format!("{:x}", Sha256::digest(&canonical)),
                std::fs::read_to_string(path.with_extension("sha256")).unwrap(),
                "{path:?}"
            );
            let policy = serde_json::from_value(value["policy"].clone()).unwrap();
            assert_eq!(
                matches(
                    &policy,
                    value["command"].as_str().unwrap(),
                    value["pty"].as_bool().unwrap(),
                    value["stdin"].as_bool().unwrap()
                ),
                value["matches"].as_bool().unwrap(),
                "{path:?}"
            );
            count += 1;
        }
        assert_eq!(count, 39, "all published cases must be present");
    }

    #[test]
    fn configured_git_gh_and_eval_stay_local() {
        for line in [
            "git status && cargo test",
            "gh pr view; cargo test",
            "eval cargo test",
        ] {
            let policy = RemoteExecPolicy {
                enabled: true,
                commands: vec![
                    "git".into(),
                    "gh".into(),
                    "eval".into(),
                    "cargo test".into(),
                ],
            };
            assert!(!matches(&policy, line, false, false), "{line}");
        }
    }

    #[test]
    fn path_qualified_forbidden_executables_cannot_be_allowlisted() {
        for name in [
            "git", "gh", "eval", "GIT", "GH", "EVAL", "git.exe", "gh.exe", "eval.exe",
        ] {
            for directory in ["/usr/bin/", "./tools/", "../tools/", "a/b/", ""] {
                let executable = format!("{directory}{name}");
                let policy = RemoteExecPolicy {
                    enabled: true,
                    commands: vec![executable.clone()],
                };
                for spelling in [
                    executable.clone(),
                    format!("'{executable}'"),
                    format!("\"{executable}\""),
                ] {
                    assert!(
                        !matches(&policy, &format!("{spelling} status"), false, false),
                        "{spelling} was admitted"
                    );
                }
                assert!(
                    !valid_prefix(&executable),
                    "forbidden executable accepted as policy prefix: {executable}"
                );
            }
        }
        let policy = RemoteExecPolicy {
            enabled: true,
            commands: vec!["/tools/notgit".into()],
        };
        assert!(matches(&policy, "/tools/notgit status", false, false));
    }

    #[test]
    fn unquoted_line_continuations_cannot_hide_forbidden_flags() {
        let policy = RemoteExecPolicy {
            enabled: true,
            commands: vec!["cargo test".into()],
        };
        for flag in ["--fix", "--run-ignored", "--ignored", "--include-ignored"] {
            for suffix in ["", "=true"] {
                let word = format!("{flag}{suffix}");
                for split in 1..word.len() {
                    let continued = format!("{}\\\n{}", &word[..split], &word[split..]);
                    for spelling in [continued.clone(), format!("\"{continued}\"")] {
                        let line = format!("cargo test -- {spelling}");
                        assert!(
                            !matches(&policy, &line, false, false),
                            "forbidden flag escaped through {line:?}"
                        );
                    }
                }
            }
        }
        assert!(matches(
            &policy,
            "cargo test -- harmless\\\nargument",
            false,
            false
        ));
    }

    #[test]
    fn policy_disabled_and_mixed_lines_stay_whole() {
        let mut policy = RemoteExecPolicy {
            enabled: true,
            commands: vec!["cd".into(), "cargo test".into()],
        };
        for (line, pty, stdin, expected) in [
            ("cargo test -p x", false, false, true),
            ("cd x && cargo test", false, false, true),
            ("cargo test && cargo fmt", false, false, false),
            ("cargo test", true, false, false),
            ("cargo test", false, true, false),
            ("cargo test < input", false, false, false),
            ("cargo testx", false, false, false),
        ] {
            assert_eq!(matches(&policy, line, pty, stdin), expected, "{line}");
        }
        policy.enabled = false;
        assert!(!matches(&policy, "cargo test", false, false));
    }
}
