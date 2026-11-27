use super::{gh_repo_env, Path, TargetRepository};

pub(super) fn canonical_repository_key(value: &str) -> Option<String> {
    let remote = value.trim().trim_end_matches('/');
    let path = if let Some(path) = [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git://github.com/",
        "git@github.com:",
        "github.com/",
    ]
    .iter()
    .find_map(|prefix| remote.strip_prefix(prefix))
    {
        path
    } else if remote.contains("://") || remote.contains('@') || remote.contains(':') {
        // Repository bindings identify GitHub repositories. A foreign remote is
        // intentionally unmapped rather than treated as an owner/name string.
        return None;
    } else {
        remote
    }
    .trim_end_matches(".git")
    .trim_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repository = parts.next()?;
    (valid_repository_component(owner)
        && valid_repository_component(repository)
        && parts.next().is_none())
    .then(|| {
        format!(
            "{}/{}",
            owner.to_ascii_lowercase(),
            repository.to_ascii_lowercase()
        )
    })
}

pub(super) fn valid_repository_component(value: &str) -> bool {
    !matches!(value, "" | "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(super) fn explicit_or_current_repository(explicit: Option<&str>, cwd: &Path) -> Option<String> {
    if let Some(repo) = explicit {
        let (owner, name) = repo.split_once('/')?;
        return (valid_repository_component(owner) && valid_repository_component(name))
            .then(|| repo.to_ascii_lowercase());
    }
    TargetRepository {
        gh_repo: gh_repo_env(),
        ..TargetRepository::default()
    }
    .repository_key(cwd)
}

pub(super) fn valid_environment(name: &str) -> bool {
    // Keep the encoded name in one URL segment, with no `{}` placeholders.
    !name.trim().is_empty()
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\', '%', '?', '#', '{', '}'])
        && !name.chars().any(char::is_control)
}

#[cfg(unix)]
pub(super) fn redirected_stdin() -> bool {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // fstat inspects stdin without reading bytes or moving the file offset.
    if unsafe { libc::fstat(libc::STDIN_FILENO, metadata.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: successful fstat initialized the result.
    matches!(
        unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT,
        libc::S_IFIFO | libc::S_IFREG
    )
}

#[cfg(not(unix))]
pub(super) fn redirected_stdin() -> bool {
    // The signed rows that use redirected stdin support macOS and Linux only.
    false
}
