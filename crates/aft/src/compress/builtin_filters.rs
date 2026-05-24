//! Built-in TOML compression filters compiled into the aft binary.
//!
//! Each filter file under `compress/builtin_filters/*.toml` is registered
//! here with its filename stem as the registry key. The filename also
//! serves as the default `[filter].matches` entry when omitted.
//!
//! Filters added later (worker D) just append entries to [`ALL`].

/// Every builtin TOML filter, as `(name, source)` pairs.
///
/// `name` is the filename stem (`make.toml` → `"make"`) and is what user
/// overrides target — `~/.config/aft/filters/make.toml` replaces the builtin
/// of the same name wholesale.
pub const ALL: &[(&str, &str)] = &[
    ("make", include_str!("builtin_filters/make.toml")),
    ("ls", include_str!("builtin_filters/ls.toml")),
    ("tree", include_str!("builtin_filters/tree.toml")),
    ("df", include_str!("builtin_filters/df.toml")),
    ("du", include_str!("builtin_filters/du.toml")),
    ("find", include_str!("builtin_filters/find.toml")),
    ("wc", include_str!("builtin_filters/wc.toml")),
    ("gradle", include_str!("builtin_filters/gradle.toml")),
    (
        "xcodebuild",
        include_str!("builtin_filters/xcodebuild.toml"),
    ),
    ("terraform", include_str!("builtin_filters/terraform.toml")),
    ("helm", include_str!("builtin_filters/helm.toml")),
    ("docker", include_str!("builtin_filters/docker.toml")),
    ("kubectl", include_str!("builtin_filters/kubectl.toml")),
    ("gh", include_str!("builtin_filters/gh.toml")),
    (
        "ansible-playbook",
        include_str!("builtin_filters/ansible-playbook.toml"),
    ),
    ("aws", include_str!("builtin_filters/aws.toml")),
    ("curl", include_str!("builtin_filters/curl.toml")),
    ("deno", include_str!("builtin_filters/deno.toml")),
    ("pip", include_str!("builtin_filters/pip.toml")),
    ("psql", include_str!("builtin_filters/psql.toml")),
    ("uv", include_str!("builtin_filters/uv.toml")),
    ("wget", include_str!("builtin_filters/wget.toml")),
];
