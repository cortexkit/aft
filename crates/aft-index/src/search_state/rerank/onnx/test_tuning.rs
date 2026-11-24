#[cfg(unix)]
pub fn experimental_tuning(
    coreml: Option<ort::ep::coreml::ComputeUnits>,
    length_buckets: bool,
    profile_prefix: Option<&'static str>,
    fixed_shape: Option<(usize, usize)>,
) -> SessionTuning {
    SessionTuning {
        coreml,
        length_buckets,
        profile_prefix,
        fixed_shape,
        ..SessionTuning::default()
    }
}

#[cfg(unix)]
pub fn profiling_tuning(
    defaults: SessionTuning,
    intra_op_spinning: bool,
    memory_pattern: bool,
    cpu_arena: bool,
    arena_shrinkage: bool,
    attention_units: usize,
) -> SessionTuning {
    SessionTuning {
        intra_op_spinning,
        memory_pattern,
        cpu_arena,
        arena_shrinkage,
        attention_units,
        ..defaults
    }
}
