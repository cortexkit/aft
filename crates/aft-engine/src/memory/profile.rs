pub use aft_base::memory::profile::{
    current, current_effective, effective, EffectiveMemory, MemoryMode, MemoryProfile,
};

/// Publish the process memory profile and refresh the dependent index limits.
pub fn publish(configured: Option<MemoryMode>) {
    aft_base::memory::profile::publish_with_notification(
        configured,
        &crate::data_layer::runtime::memory_profile_changed,
    );
}
