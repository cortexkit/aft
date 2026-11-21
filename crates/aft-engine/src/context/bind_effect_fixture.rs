use super::*;

/// Which configure side effect a test counts per (root, session).
#[derive(Clone, Copy)]
pub(super) enum ConfigureBindEffect {
    Session,
    Maintenance,
}

fn configure_bind_effects(
) -> &'static parking_lot::Mutex<BTreeMap<(PathBuf, String), (usize, usize)>> {
    static EFFECTS: std::sync::OnceLock<
        parking_lot::Mutex<BTreeMap<(PathBuf, String), (usize, usize)>>,
    > = std::sync::OnceLock::new();
    EFFECTS.get_or_init(|| parking_lot::Mutex::new(BTreeMap::new()))
}

pub(super) fn record_configure_bind_effect_for_test(
    root: &Path,
    session_id: &str,
    effect: ConfigureBindEffect,
) {
    let mut effects = configure_bind_effects().lock();
    let counts = effects
        .entry((root.to_path_buf(), session_id.to_string()))
        .or_default();
    match effect {
        ConfigureBindEffect::Session => counts.0 += 1,
        ConfigureBindEffect::Maintenance => counts.1 += 1,
    }
}

/// How many times configure recorded `session_id` as bound to `root`, and how
/// many configure maintenance jobs it queued for that pair. Tests use roots of
/// their own, so parallel tests do not share entries.
pub(crate) fn configure_bind_effects_for_test(root: &Path, session_id: &str) -> (usize, usize) {
    configure_bind_effects()
        .lock()
        .get(&(root.to_path_buf(), session_id.to_string()))
        .copied()
        .unwrap_or((0, 0))
}
