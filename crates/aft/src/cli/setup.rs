//! `aft setup`: emit the feature plan, or write the user's base choices.
//!
//! ```text
//! aft setup --plan [--harness <id>] [--plan-version 1]
//! aft setup --answers <file|-> [--harness <id>]
//! aft setup --yes [--harness <id>]
//! ```
//!
//! The plan goes to stdout as one JSON document; warnings go to stderr so the
//! plan stays machine-readable. Any configuration that ordinary loading would
//! reject (removed keys, already-retired GitHub aliases) fails every mode
//! before anything is printed or written, and points at `aft doctor --fix`.
//! Writes touch only the user file `~/.config/cortexkit/aft.jsonc`.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use aft::feature_config::{self, PolicyPhase};
use aft::setup_plan::{
    derive_plan, parse_answers, read_inputs, render_setup, unknown_disabled_warning,
    FeatureObserver, NoRuntimeObservation, SetupHarness, SetupSelections, PLAN_VERSION,
    SETUP_HARNESS_ENV, UNSUPPORTED_PLAN_VERSION,
};

const USAGE: &str = "usage: aft setup (--plan [--plan-version 1] | --answers <file|-> | --yes) [--harness opencode|pi|omp]";

#[derive(Debug)]
pub struct SetupError {
    message: String,
    code: i32,
}

impl SetupError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: format!("{}\n{USAGE}", message.into()),
            code: 2,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.code
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Plan,
    Answers(String),
    Yes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    mode: Mode,
    harness: Option<SetupHarness>,
    /// Set by a caller that already reported this operation's load warnings
    /// (the wizard's plan call), so one setup operation warns once.
    quiet_load_warnings: bool,
}

fn parse_args(args: Vec<OsString>, env_harness: Option<String>) -> Result<Args, SetupError> {
    let mut modes = Vec::new();
    let mut harness_flag = None;
    let mut plan_version = None;
    let mut quiet_load_warnings = false;
    let mut args = args
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| SetupError::usage(format!("{name} needs a value")))
        };
        match flag.as_str() {
            "--plan" => modes.push(Mode::Plan),
            "--yes" | "-y" => modes.push(Mode::Yes),
            "--answers" => modes.push(Mode::Answers(value("--answers")?)),
            "--harness" => harness_flag = Some(value("--harness")?),
            "--plan-version" => plan_version = Some(value("--plan-version")?),
            "--no-load-warnings" => quiet_load_warnings = true,
            other => return Err(SetupError::usage(format!("unknown argument {other}"))),
        }
    }
    let mode = match modes.as_slice() {
        [mode] => mode.clone(),
        [] => {
            return Err(SetupError::usage(
                "choose one of --plan, --answers or --yes",
            ))
        }
        _ if modes.iter().any(|mode| *mode == Mode::Yes)
            && modes.iter().any(|mode| matches!(mode, Mode::Answers(_))) =>
        {
            return Err(SetupError::usage(
                "--yes and --answers are mutually exclusive",
            ))
        }
        _ => {
            return Err(SetupError::usage(
                "choose only one of --plan, --answers or --yes",
            ))
        }
    };
    if let Some(version) = plan_version {
        if mode != Mode::Plan {
            return Err(SetupError::usage("--plan-version applies only to --plan"));
        }
        if version.trim() != PLAN_VERSION.to_string() {
            return Err(SetupError::failed(UNSUPPORTED_PLAN_VERSION));
        }
    }
    // An explicit selector wins over the invoking adapter's context.
    let harness = harness_flag
        .or_else(|| env_harness.filter(|value| !value.is_empty()))
        .map(|value| SetupHarness::from_selector(&value).map_err(SetupError::usage))
        .transpose()?;
    Ok(Args {
        mode,
        harness,
        quiet_load_warnings,
    })
}

/// Filesystem locations for one invocation.
pub struct SetupPaths {
    pub cwd: PathBuf,
    pub user_config_path: Option<PathBuf>,
}

pub fn run(args: Vec<OsString>) -> Result<(), SetupError> {
    let cwd = std::env::current_dir().map_err(|error| {
        SetupError::failed(format!("could not determine current directory: {error}"))
    })?;
    let paths = SetupPaths {
        cwd,
        user_config_path: aft::subc_config::cortexkit_user_config_path(),
    };
    run_with(
        args,
        std::env::var(SETUP_HARNESS_ENV).ok(),
        &paths,
        &NoRuntimeObservation,
        feature_config::current_policy_phase(),
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

fn read_answers(source: &str, stdin: &mut dyn Read) -> Result<String, SetupError> {
    if source == "-" {
        let mut text = String::new();
        stdin.read_to_string(&mut text).map_err(|error| {
            SetupError::failed(format!("could not read answers from stdin: {error}"))
        })?;
        Ok(text)
    } else {
        std::fs::read_to_string(source).map_err(|error| {
            SetupError::failed(format!("could not read answers {source}: {error}"))
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn run_with(
    args: Vec<OsString>,
    env_harness: Option<String>,
    paths: &SetupPaths,
    observer: &dyn FeatureObserver,
    phase: PolicyPhase,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), SetupError> {
    let args = parse_args(args, env_harness)?;
    let selections = match &args.mode {
        Mode::Plan => None,
        Mode::Yes => Some(SetupSelections::Yes),
        Mode::Answers(source) => Some(SetupSelections::Answers(
            parse_answers(&read_answers(source, stdin)?).map_err(SetupError::failed)?,
        )),
    };

    let inputs =
        read_inputs(paths.user_config_path.as_deref(), &paths.cwd).map_err(SetupError::failed)?;
    let outcome = derive_plan(&inputs, args.harness, observer, phase).map_err(|errors| {
        SetupError::failed(format!(
            "{}\nAFT cannot load this configuration. Run `aft doctor --fix` to migrate it, then rerun setup.",
            errors.join("\n")
        ))
    })?;

    let report_unknown = |stderr: &mut dyn Write, names: &[String]| {
        if !args.quiet_load_warnings {
            if let Some(warning) = unknown_disabled_warning(names) {
                let _ = writeln!(stderr, "warning: {warning}");
            }
        }
    };

    let Some(selections) = selections else {
        let json = serde_json::to_string_pretty(&outcome.plan)
            .map_err(|error| SetupError::failed(format!("could not serialize plan: {error}")))?;
        report_unknown(stderr, &outcome.unknown_disabled_tools);
        writeln!(stdout, "{json}").map_err(|error| SetupError::failed(error.to_string()))?;
        return Ok(());
    };

    let Some(user_path) = paths.user_config_path.as_deref() else {
        return Err(SetupError::failed(
            "could not locate the user config directory (set HOME or XDG_CONFIG_HOME)",
        ));
    };
    let existing = inputs.user.as_ref().map(|file| file.text.as_str());
    let write = render_setup(existing, &outcome.plan, &selections).map_err(|error| {
        SetupError::failed(format!("could not update {}: {error}", user_path.display()))
    })?;
    report_unknown(stderr, &outcome.unknown_disabled_tools);
    write_user_file(user_path, existing, &write.text)?;
    let summary = serde_json::json!({ "written": user_path.to_string_lossy() });
    writeln!(stdout, "{summary}").map_err(|error| SetupError::failed(error.to_string()))?;
    Ok(())
}

fn write_user_file(path: &Path, existing: Option<&str>, text: &str) -> Result<(), SetupError> {
    if existing == Some(text) {
        return Ok(());
    }
    aft::jsonc_edit::write_atomic(path, text)
        .map_err(|error| SetupError::failed(format!("could not write {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aft::setup_plan::{Effective, IndexObservation, IndexPlane};

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: SetupPaths,
        project: PathBuf,
    }

    fn fixture(user: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let user_path = dir
            .path()
            .join("config")
            .join("cortexkit")
            .join("aft.jsonc");
        if let Some(text) = user {
            std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
            std::fs::write(&user_path, text).unwrap();
        }
        Fixture {
            paths: SetupPaths {
                cwd: project.clone(),
                user_config_path: Some(user_path),
            },
            project,
            _dir: dir,
        }
    }

    fn invoke(
        fixture: &Fixture,
        args: &[&str],
        env_harness: Option<&str>,
        stdin: &str,
    ) -> (Result<(), SetupError>, String, String) {
        invoke_with(fixture, args, env_harness, stdin, &NoRuntimeObservation)
    }

    fn invoke_with(
        fixture: &Fixture,
        args: &[&str],
        env_harness: Option<&str>,
        stdin: &str,
        observer: &dyn FeatureObserver,
    ) -> (Result<(), SetupError>, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = run_with(
            args.iter().map(OsString::from).collect(),
            env_harness.map(str::to_string),
            &fixture.paths,
            observer,
            PolicyPhase::Window,
            &mut stdin.as_bytes(),
            &mut stdout,
            &mut stderr,
        );
        (
            result,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    fn user_text(fixture: &Fixture) -> Option<String> {
        std::fs::read_to_string(fixture.paths.user_config_path.as_ref().unwrap()).ok()
    }

    fn plan_row(stdout: &str, id: &str) -> serde_json::Value {
        let plan: serde_json::Value = serde_json::from_str(stdout).unwrap();
        plan["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()
            .clone()
    }

    #[test]
    fn plan_prints_json_and_warns_about_unknown_names_on_stderr_only() {
        let fixture = fixture(Some(r#"{"disabled_tools": ["aft_move", "typo_name"]}"#));
        let (result, stdout, stderr) = invoke(&fixture, &["--plan"], None, "");
        result.unwrap();
        let plan: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(plan["plan_version"], 1);
        assert_eq!(plan["features"].as_array().unwrap().len(), 28);
        assert_eq!(stderr.matches("unknown_disabled_tools").count(), 1);
        assert!(stderr.contains("typo_name"));
        let (_, _, quiet) = invoke(&fixture, &["--plan", "--no-load-warnings"], None, "");
        assert!(quiet.is_empty());
    }

    #[test]
    fn unknown_harness_and_plan_versions_fail_without_output_or_writes() {
        let fixture = fixture(None);
        for (args, env) in [
            (vec!["--plan", "--harness", "opencode-v2"], None),
            (vec!["--plan"], Some("runner")),
            (vec!["--yes", "--harness=omp2"], None),
            (vec!["--plan", "--plan-version", "2"], None),
            (vec!["--yes", "--answers", "-"], None),
            (vec![], None),
        ] {
            let (result, stdout, _) = invoke(&fixture, &args, env, "");
            assert!(result.is_err(), "{args:?} {env:?}");
            assert!(stdout.is_empty(), "{args:?}");
        }
        let (result, _, _) = invoke(&fixture, &["--plan", "--plan-version", "2"], None, "");
        assert_eq!(result.unwrap_err().to_string(), UNSUPPORTED_PLAN_VERSION);
        assert_eq!(user_text(&fixture), None);
    }

    #[test]
    fn explicit_harness_flag_wins_over_the_invocation_context() {
        let fixture = fixture(Some(
            r#"{"harnesses": {"opencode": {"disabled_tools": ["aft_zoom"]}, "pi": {"disabled_tools": ["aft_outline"]}}}"#,
        ));
        let (_, from_env, _) = invoke(&fixture, &["--plan"], Some("pi"), "");
        assert_eq!(plan_row(&from_env, "aft_outline")["effective"], "off");
        assert_eq!(plan_row(&from_env, "aft_zoom")["effective"], "ready");
        let (_, from_flag, _) = invoke(
            &fixture,
            &["--plan", "--harness", "opencode"],
            Some("pi"),
            "",
        );
        assert_eq!(plan_row(&from_flag, "aft_zoom")["effective"], "off");
        assert_eq!(plan_row(&from_flag, "aft_outline")["effective"], "ready");
        let (_, neither, _) = invoke(&fixture, &["--plan"], None, "");
        assert_eq!(plan_row(&neither, "aft_zoom")["effective"], "ready");
        assert_eq!(plan_row(&neither, "aft_outline")["effective"], "ready");
    }

    #[test]
    fn project_discovery_starts_at_the_invocation_directory() {
        let fixture = fixture(Some("{}"));
        let (_, outside, _) = invoke(&fixture, &["--plan"], None, "");
        std::fs::create_dir_all(fixture.project.join(".cortexkit")).unwrap();
        std::fs::write(
            fixture.project.join(".cortexkit/aft.jsonc"),
            r#"{"disabled_tools": ["aft_zoom"]}"#,
        )
        .unwrap();
        let (_, inside, _) = invoke(&fixture, &["--plan"], None, "");
        assert_eq!(plan_row(&outside, "aft_zoom")["effective"], "ready");
        assert_eq!(plan_row(&inside, "aft_zoom")["effective"], "off");
        assert_eq!(plan_row(&inside, "aft_zoom")["source"], "default");
    }

    #[test]
    fn rejected_configuration_fails_every_mode_without_output_or_writes() {
        let original = r#"{"gh_shim": {"enabled": false}}"#;
        let fixture = fixture(Some(original));
        for args in [vec!["--plan"], vec!["--yes"], vec!["--answers", "-"]] {
            let (result, stdout, _) = invoke(
                &fixture,
                &args,
                None,
                r#"{"plan_version": 1, "selections": {"aft_move": true}}"#,
            );
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("removed_config_key:gh_shim:use:github.shim"),
                "{error}"
            );
            assert!(error.contains("aft doctor --fix"), "{error}");
            assert!(stdout.is_empty());
            assert_eq!(user_text(&fixture).as_deref(), Some(original));
        }
    }

    #[test]
    fn answers_write_the_user_file_and_bad_answers_write_nothing() {
        let original = "{\n  // mine\n  \"edit_mode\": \"hashline\"\n}\n";
        let fixture = fixture(Some(original));
        for bad in [
            r#"{"plan_version": 1, "selections": {"aft_nope": true}}"#,
            r#"{"plan_version": 1, "selections": {"aft_move": 1}}"#,
            r#"{"plan_version": 9, "selections": {}}"#,
        ] {
            let (result, stdout, _) = invoke(&fixture, &["--answers", "-"], None, bad);
            assert!(result.is_err(), "{bad}");
            assert!(stdout.is_empty());
            assert_eq!(user_text(&fixture).as_deref(), Some(original));
        }
        let (result, _, _) = invoke(
            &fixture,
            &["--answers", "-"],
            None,
            r#"{"plan_version": 1, "selections": {"aft_move": true, "aft_delete": true}}"#,
        );
        result.unwrap();
        let text = user_text(&fixture).unwrap();
        assert!(
            text.contains("// mine") && text.contains("\"disabled_tools\": []"),
            "{text}"
        );
    }

    #[test]
    fn yes_creates_a_missing_user_file_with_proposed_defaults() {
        struct Ready;
        impl FeatureObserver for Ready {
            fn index(&self, _plane: IndexPlane) -> IndexObservation {
                IndexObservation {
                    effective: Effective::Ready,
                    unavailable_reason: None,
                }
            }
        }
        let fixture = fixture(None);
        let (result, stdout, _) = invoke_with(&fixture, &["--yes"], None, "", &Ready);
        result.unwrap();
        assert!(stdout.contains("written"));
        let written: serde_json::Value =
            serde_json::from_str(&user_text(&fixture).unwrap()).unwrap();
        assert_eq!(
            written["disabled_tools"],
            serde_json::json!(["aft_delete", "aft_move"])
        );
        assert_eq!(written["indexes"]["semantic"], true);
    }
}
