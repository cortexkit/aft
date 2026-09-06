//! Environment lookup guards shared by path and identity resolvers.

use std::ffi::OsString;

/// Read an environment value only when it contains at least one byte/code unit.
/// Environment-derived path ladders treat an empty assignment like an unset rung.
#[doc(hidden)]
pub fn non_empty_os_var(name: &str) -> Option<OsString> {
    non_empty_os_var_with(name, |key| std::env::var_os(key))
}

/// Injected form used by resolver tests so they never mutate process-global state.
#[doc(hidden)]
pub fn non_empty_os_var_with(
    name: &str,
    lookup: impl FnOnce(&str) -> Option<OsString>,
) -> Option<OsString> {
    lookup(name).filter(|value| !value.is_empty())
}

/// UTF-8 counterpart for environment-derived identifiers and textual paths.
#[doc(hidden)]
pub fn non_empty_var(name: &str) -> Option<String> {
    non_empty_var_with(name, |key| std::env::var(key).ok())
}

/// Injected UTF-8 form used by resolver tests.
#[doc(hidden)]
pub fn non_empty_var_with(
    name: &str,
    lookup: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    lookup(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_lookup_treats_empty_environment_values_as_unset() {
        assert_eq!(
            non_empty_os_var_with("PATH_RUNG", |key| {
                assert_eq!(key, "PATH_RUNG");
                Some(OsString::new())
            }),
            None
        );
        assert_eq!(
            non_empty_os_var_with("PATH_RUNG", |_| Some(OsString::from("relative/path"))),
            Some(OsString::from("relative/path"))
        );
        assert_eq!(
            non_empty_var_with("NAME_RUNG", |_| Some(String::new())),
            None
        );
        assert_eq!(
            non_empty_var_with("NAME_RUNG", |_| Some("host-name".to_string())),
            Some("host-name".to_string())
        );
    }
}
