use std::ffi::OsString;
use std::path::PathBuf;

/// Parse the detached PATH-probe helper's sole required cache-file argument.
pub fn parse_cache_path<I, S>(args: I) -> Result<PathBuf, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let Some(cache_path) = args.next() else {
        return Err("missing cache file for --probe-login-shell-path".to_string());
    };
    if args.next().is_some() {
        return Err("--probe-login-shell-path accepts exactly one cache file".to_string());
    }
    if cache_path.is_empty() {
        return Err("cache file for --probe-login-shell-path must not be empty".to_string());
    }
    Ok(PathBuf::from(cache_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_cache_file() {
        assert_eq!(
            parse_cache_path([OsString::from("/tmp/effective-path.json")]).unwrap(),
            PathBuf::from("/tmp/effective-path.json")
        );
    }

    #[test]
    fn rejects_missing_or_extra_arguments() {
        assert!(parse_cache_path(Vec::<OsString>::new()).is_err());
        assert!(
            parse_cache_path([OsString::from("one.json"), OsString::from("two.json")]).is_err()
        );
    }
}
