use super::*;

#[cfg(unix)]
pub struct Runtime {
    pub(super) proxy: SecretProxy,
    pub(super) bundle: PathBuf,
    pub(crate) lifetime: crate::secret::SecretScope,
    pub(crate) authorization: crate::secret::SecretScope,
    pub(super) armed: bool,
}
