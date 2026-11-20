//! Memory mode values and platform limit tables shared by configuration and runtime.

use serde::{Deserialize, Serialize};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryMode {
    Lean,
    Balanced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryProfile {
    /// None lets the resolver use all available CPUs.
    pub resolver_threads: Option<usize>,
    /// None restores the allocator's startup defaults.
    pub decay_ms: Option<isize>,
    pub block_cache_bytes: usize,
    /// Estimated bytes of decoded parses and new bound surfaces in a cold batch.
    pub bind_batch_bytes: usize,
    pub refresh_floor_bytes: u64,
}

impl MemoryMode {
    pub const fn profile(self) -> MemoryProfile {
        self.profile_for_platform(cfg!(windows), cfg!(target_os = "linux"))
    }

    const fn profile_for_platform(self, windows: bool, linux: bool) -> MemoryProfile {
        match self {
            Self::Lean => MemoryProfile {
                resolver_threads: if windows { None } else { Some(4) },
                decay_ms: if linux { Some(0) } else { None },
                block_cache_bytes: 128 * MIB,
                bind_batch_bytes: 384 * MIB,
                refresh_floor_bytes: GIB / 2,
            },
            Self::Balanced => MemoryProfile {
                resolver_threads: None,
                decay_ms: None,
                block_cache_bytes: 256 * MIB,
                bind_batch_bytes: 768 * MIB,
                refresh_floor_bytes: GIB,
            },
        }
    }

    pub fn automatic(total_ram_bytes: Option<u64>) -> Self {
        match total_ram_bytes {
            Some(bytes) if bytes < 8 * GIB => Self::Lean,
            _ => Self::Balanced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_profiles_match_the_limit_table() {
        for (platform, windows, linux, threads, decay) in [
            ("Linux", false, true, Some(4), Some(0)),
            ("macOS", false, false, Some(4), None),
            ("Windows", true, false, None, None),
        ] {
            assert_eq!(
                MemoryMode::Lean.profile_for_platform(windows, linux),
                MemoryProfile {
                    resolver_threads: threads,
                    decay_ms: decay,
                    block_cache_bytes: 128 * MIB,
                    bind_batch_bytes: 384 * MIB,
                    refresh_floor_bytes: 512 * 1024 * 1024,
                },
                "{platform} lean",
            );
            assert_eq!(
                MemoryMode::Balanced.profile_for_platform(windows, linux),
                MemoryProfile {
                    resolver_threads: None,
                    decay_ms: None,
                    block_cache_bytes: 256 * MIB,
                    bind_batch_bytes: 768 * MIB,
                    refresh_floor_bytes: 1024 * 1024 * 1024,
                },
                "{platform} balanced",
            );
        }
        for mode in [MemoryMode::Lean, MemoryMode::Balanced] {
            assert_eq!(
                mode.profile(),
                mode.profile_for_platform(cfg!(windows), cfg!(target_os = "linux"))
            );
        }
    }
    #[test]
    fn memory_automatic_thresholds_and_unavailable_ram() {
        for (bytes, expected) in [
            (Some(79 * GIB / 10), MemoryMode::Lean),
            (Some(8 * GIB), MemoryMode::Balanced),
            (Some(319 * GIB / 10), MemoryMode::Balanced),
            (Some(32 * GIB), MemoryMode::Balanced),
            (Some(128 * GIB), MemoryMode::Balanced),
            (None, MemoryMode::Balanced),
        ] {
            assert_eq!(MemoryMode::automatic(bytes), expected);
        }
    }
    #[test]
    fn memory_fast_is_an_invalid_value() {
        assert!(serde_json::from_str::<MemoryMode>("\"fast\"").is_err());
    }
}
