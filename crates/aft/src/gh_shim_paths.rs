use std::path::Path;

pub const SHIMS_DIR_NAME: &str = "shims";
pub(crate) const GH_SHIMS_DIR_ENV: &str = "AFT_GH_SHIMS_DIR";
pub(crate) const GH_SHIM_BINARY_ENV: &str = "AFT_GH_SHIM_BINARY";

/// Environment variable that carries the ticket into a spawned command.
pub const GH_SHIM_TICKET_ENV: &str = "AFT_GH_SHIM_TICKET";
/// Restriction hint added only by worker launch planning. It cannot grant identity.
pub const GH_SHIM_WORKER_ENV: &str = "AFT_GH_SHIM_WORKER";

/// Render the Windows command wrapper separately so its quoting contract can be
/// checked on every development platform; `cmd.exe` dispatch still requires
/// the native Windows CI oracle.
pub fn windows_gh_cmd(binary: &Path) -> Vec<u8> {
    let rendered = binary.to_string_lossy();
    debug_assert!(!rendered.contains('"'));
    let rendered = rendered.replace('%', "%%");
    format!("@echo off\r\n\"{rendered}\" gh-shim %*\r\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_wrapper_uses_the_explicit_gh_shim_dispatch_form() {
        assert_eq!(
            String::from_utf8(windows_gh_cmd(Path::new(r"C:\AFT Dev\aft.exe"))).unwrap(),
            "@echo off\r\n\"C:\\AFT Dev\\aft.exe\" gh-shim %*\r\n"
        );
    }
}
