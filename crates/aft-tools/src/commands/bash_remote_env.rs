//! Which configured `bash.env` values reach a `runon` build-server job.
//!
//! A `bash.env` value is meant for this machine's shells. A build server is a
//! shared machine where the project's environment is neither needed nor
//! trusted, so only the user tier can opt a name in, through
//! `bash.runon_env: ["NAME", ...]`. A project value never leaves this machine,
//! even when its name is listed. Everything else a job does not receive is
//! named in the job's disclosure line.

use std::collections::BTreeMap;

use crate::config::Config;
use crate::config_resolve::ConfiguredBashEnv;

// The four once-per-session notices are user-facing copy that was reviewed and
// approved as written. Each lives here, in one place, and a test asserts its
// exact text, so a wording change is a deliberate edit in a single spot.

/// What the notice says before the applied names. Each name follows as
/// `NAME (tier)`, joined with `, `.
pub(crate) const APPLIED_NOTICE_PREFIX: &str =
    "Configured bash.env keys applied (values not shown):";

/// What the notice says before the names a user value shadows, joined with
/// `, `.
pub(crate) const SHADOWED_PROJECT_NOTICE_PREFIX: &str = "Project value for";

/// The note that says a project's `bash.env` did not apply because the project
/// is not on the trust list, with the documented command for trusting it.
pub(crate) const PROJECT_UNTRUSTED_NOTICE: &str =
    "Project bash.env ignored: project not trusted (run npx @cortexkit/aft@latest doctor filters trust).";

/// What the notice says after the shadowed names.
pub(crate) const SHADOWED_PROJECT_NOTICE_SUFFIX: &str = "ignored: user value wins.";

/// What the notice says about a `runon` build-server job. `{}` takes the
/// comma-joined names the job does not receive.
pub(crate) const RUNON_WITHHELD_NOTICE: &str = "The runon build-server job does not receive these bash.env values: {} (list a user-tier name in bash.runon_env to send it; project values are never sent).";

/// The user-tier names a runon job may receive, and the configuration this
/// decision was made from. The same modules used to be read directly from
/// `bash.env`, so a name is forwarded only when the user set it and the user
/// listed it.
#[derive(Debug, Default, Clone)]
pub(crate) struct RemoteEnvPlan {
    /// User `bash.env` values whose names appear in `bash.runon_env`.
    pub(crate) forwarded: BTreeMap<String, String>,
    /// Configured names that were not forwarded, with the reason in the
    /// disclosure line's wording.
    pub(crate) withheld: Vec<String>,
}

/// Build the runon env plan from the resolved configuration.
pub(crate) fn plan(config: &Config) -> RemoteEnvPlan {
    let mut forwarded = BTreeMap::new();
    let mut withheld = Vec::new();
    for (name, value) in &config.bash.env {
        if config.bash.runon_env.iter().any(|listed| listed == name) {
            forwarded.insert(name.clone(), value.clone());
        } else {
            withheld.push(name.clone());
        }
    }
    // A project value is never sent, even when its name is listed.
    for name in config.bash.project_env.keys() {
        if !withheld.contains(name) {
            withheld.push(name.clone());
        }
    }
    RemoteEnvPlan {
        forwarded,
        withheld,
    }
}

/// The subset of one tier's configured map that this job may receive.
pub(crate) fn forwarded_names(
    requested: &RemoteEnvPlan,
    configured: &ConfiguredBashEnv,
) -> BTreeMap<String, String> {
    match configured.tier {
        // A listed user name is forwarded with its user value; a listed name
        // the user did not set is not invented here.
        crate::bash_env::BashEnvTier::User => configured
            .names
            .iter()
            .filter(|(name, _)| requested.forwarded.contains_key(*name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        // Project values never leave the machine.
        crate::bash_env::BashEnvTier::Project => BTreeMap::new(),
    }
}

/// What the once-per-session notice says about configured environment values.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Disclosure {
    /// One line per notice.
    pub(crate) lines: Vec<String>,
}

impl Disclosure {
    /// Build the notice from what applied, what the user tier shadowed, whether
    /// an untrusted project supplied values, and what a runon job does not
    /// receive. Values never appear: only names and their tier.
    pub(crate) fn build(
        applied: &[ConfiguredBashEnv],
        shadowed_project_names: &[String],
        project_untrusted: bool,
        remote: &RemoteEnvPlan,
    ) -> Self {
        let mut lines = Vec::new();
        let applied_names = format_applied(applied);
        if !applied_names.is_empty() {
            lines.push(format!("{APPLIED_NOTICE_PREFIX} {applied_names}."));
        }
        if project_untrusted {
            lines.push(PROJECT_UNTRUSTED_NOTICE.to_string());
        }
        if !shadowed_project_names.is_empty() {
            lines.push(format!(
                "{SHADOWED_PROJECT_NOTICE_PREFIX} {} {SHADOWED_PROJECT_NOTICE_SUFFIX}",
                shadowed_project_names.join(", ")
            ));
        }
        if !remote.withheld.is_empty() {
            let mut sorted = remote.withheld.clone();
            sorted.sort();
            sorted.dedup();
            lines.push(RUNON_WITHHELD_NOTICE.replace("{}", &sorted.join(", ")));
        }
        Self { lines }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

fn format_applied(applied: &[ConfiguredBashEnv]) -> String {
    let mut tagged: Vec<(String, &'static str)> = applied
        .iter()
        .flat_map(|configured| {
            configured
                .names
                .keys()
                .map(move |name| (name.clone(), configured.tier.label()))
        })
        .collect();
    tagged.sort();
    tagged
        .into_iter()
        .map(|(name, tier)| format!("{name} ({tier})"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(env: &[(&str, &str)], project_env: &[(&str, &str)], runon_env: &[&str]) -> Config {
        let mut config = Config::default();
        config.bash.env = env
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        config.bash.project_env = project_env
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        config.bash.runon_env = runon_env.iter().map(|name| (*name).to_string()).collect();
        config
    }

    #[test]
    fn only_listed_user_names_are_sent_and_project_values_never_are() {
        let config = config(
            &[("TZ", "UTC"), ("IBToolNeverDeque", "1")],
            &[("PROJECT_ONLY", "project"), ("TZ", "Mars")],
            &["TZ", "PROJECT_ONLY"],
        );
        let plan = plan(&config);
        assert_eq!(plan.forwarded.get("TZ").map(String::as_str), Some("UTC"));
        assert!(!plan.forwarded.contains_key("IBToolNeverDeque"));
        // The project's value for TZ is never sent, listed or not.
        assert!(!plan.forwarded.contains_key("PROJECT_ONLY"));
        let project = ConfiguredBashEnv {
            names: config.bash.project_env.clone(),
            tier: crate::bash_env::BashEnvTier::Project,
        };
        assert!(forwarded_names(&plan, &project).is_empty());
    }

    /// The four notices are approved user-facing copy. This pins their exact
    /// text, so a wording change has to be a deliberate edit here too.
    #[test]
    fn the_four_notices_keep_their_approved_wording() {
        let applied = vec![
            ConfiguredBashEnv {
                names: [("TZ".to_string(), "UTC".to_string())]
                    .into_iter()
                    .collect(),
                tier: crate::bash_env::BashEnvTier::User,
            },
            ConfiguredBashEnv {
                names: [("IBToolNeverDeque".to_string(), "1".to_string())]
                    .into_iter()
                    .collect(),
                tier: crate::bash_env::BashEnvTier::Project,
            },
        ];
        let mut plan = RemoteEnvPlan::default();
        plan.withheld = vec!["BUILD_ONLY".to_string()];
        let disclosure = Disclosure::build(&applied, &["TZ".to_string()], true, &plan);
        assert_eq!(
            disclosure.lines,
            vec![
                "Configured bash.env keys applied (values not shown): IBToolNeverDeque (project), TZ (user)."
                    .to_string(),
                "Project bash.env ignored: project not trusted (run npx @cortexkit/aft@latest doctor filters trust)."
                    .to_string(),
                "Project value for TZ ignored: user value wins.".to_string(),
                "The runon build-server job does not receive these bash.env values: BUILD_ONLY (list a user-tier name in bash.runon_env to send it; project values are never sent)."
                    .to_string(),
            ]
        );
    }

    #[test]
    fn disclosure_names_what_the_job_does_not_receive() {
        let config = config(&[("TZ", "UTC")], &[("IBToolNeverDeque", "1")], &[]);
        let plan = plan(&config);
        let disclosure = Disclosure::build(&[], &[], false, &plan);
        let text = disclosure.lines.join("\n");
        assert!(text.contains("does not receive"), "{text}");
        assert!(text.contains("TZ"), "{text}");
        assert!(text.contains("IBToolNeverDeque"), "{text}");
        assert!(!text.contains("UTC"), "{text}");
    }

    #[test]
    fn names_are_tagged_with_their_tier_and_no_value_appears() {
        let applied = vec![
            ConfiguredBashEnv {
                names: [("TZ".to_string(), "UTC".to_string())]
                    .into_iter()
                    .collect(),
                tier: crate::bash_env::BashEnvTier::User,
            },
            ConfiguredBashEnv {
                names: [("IBToolNeverDeque".to_string(), "1".to_string())]
                    .into_iter()
                    .collect(),
                tier: crate::bash_env::BashEnvTier::Project,
            },
        ];
        let disclosure = Disclosure::build(&applied, &[], false, &RemoteEnvPlan::default());
        let text = disclosure.lines.join("\n");
        assert!(
            text.contains("IBToolNeverDeque (project), TZ (user)"),
            "{text}"
        );
        assert!(!text.contains("UTC"), "{text}");
        assert!(!text.contains("1,"), "{text}");
    }

    #[test]
    fn untrusted_and_shadowed_notes_explain_why_a_value_did_not_apply() {
        let applied = vec![ConfiguredBashEnv {
            names: [("USER_ONLY".to_string(), "x".to_string())]
                .into_iter()
                .collect(),
            tier: crate::bash_env::BashEnvTier::User,
        }];
        let disclosure = Disclosure::build(
            &applied,
            &["TZ".to_string()],
            true,
            &RemoteEnvPlan::default(),
        );
        let text = disclosure.lines.join("\n");
        assert!(text.contains(PROJECT_UNTRUSTED_NOTICE), "{text}");
        assert!(
            text.contains("Project value for TZ ignored: user value wins."),
            "{text}"
        );
    }
}
