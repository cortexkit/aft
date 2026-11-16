//! Process-lifetime display decisions. Fixed labels keep health output bounded
//! and never expose call keys, paths, principals, or store response text.
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counters {
    ($($name:ident => $label:literal),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub(in crate::subc::rich_ui) enum Counter { $($name),+ }
        static COUNTS: [AtomicU64; counters!(@count $($name),+)] =
            [const { AtomicU64::new(0) }; counters!(@count $($name),+)];
        pub(super) fn snapshot() -> serde_json::Value {
            let mut counts = serde_json::Map::new();
            $(let value = COUNTS[Counter::$name as usize].load(Ordering::Relaxed);
              if value != 0 { counts.insert($label.into(), value.into()); })+
            serde_json::Value::Object(counts)
        }
        #[cfg(test)]
        #[test]
        fn display_counter_labels_and_max_values_are_bounded() {
            let maximum = serde_json::json!({$($label: u64::MAX),+});
            assert!(serde_json::to_vec(&maximum).unwrap().len() < 4096);
        }
    };
    (@count $($name:ident),+) => { [$(stringify!($name)),+].len() };
}

counters! {
    RegisterNoCallKey => "register_no_call_key",
    RegisterDisabled => "register_disabled",
    RegisterOptOut => "register_opt_out",
    RegisterUnsupportedTool => "register_unsupported_tool",
    RegisterUnattested => "register_unattested",
    RegisterNotReserved => "register_not_reserved",
    RegisterInnerCall => "register_inner_call_without_origin",
    RegisterMissingIdentity => "register_missing_key_or_pin",
    RegisterInvalidCallKey => "register_invalid_call_key",
    RegisterInvalidPin => "register_invalid_pin",
    RegisterUnknownView => "register_unknown_view",
    RegisterCallCap => "register_call_cap",
    RegisterLock => "register_lock_failed",
    RegisterExisting => "register_existing",
    Registered => "registered",
    EmitUnknownView => "emit_unknown_view",
    EmitNotObject => "emit_not_object",
    EmitInvalidRecord => "emit_invalid_record",
    EmitInvalidCompletion => "emit_invalid_completion",
    EmitInvalidDetail => "emit_invalid_detail",
    EmitNoSender => "emit_no_sender",
    Emitted => "emitted",
    EnqueueLimitsLock => "enqueue_limits_lock_failed",
    EnqueueDetailTooLarge => "enqueue_detail_too_large",
    EnqueueOutboxLock => "enqueue_outbox_lock_failed",
    WakeCoalesced => "wake_coalesced",
    WakeClosed => "wake_closed",
    AdmitNotObject => "admit_not_object",
    AdmitInvalidWrite => "admit_invalid_write",
    AdmitInvalidKey => "admit_invalid_idempotency_key",
    AdmitArgumentsEncoding => "admit_arguments_encoding_failed",
    AdmitDuplicate => "admit_duplicate",
    AdmitFrameEncoding => "admit_frame_encoding_failed",
    AdmitFrameTooLarge => "admit_frame_too_large",
    AdmitCallCap => "admit_call_cap",
    AdmitEntryCap => "admit_entry_cap",
    AdmitByteCap => "admit_byte_cap",
    AdmitShedDetail => "admit_shed_detail",
    Admitted => "admitted",
    Expired => "expired",
    AttemptsExhausted => "attempts_exhausted",
    RetryCapacity => "retry_capacity",
    Retried => "retried",
    StartDisabled => "start_disabled",
    StartNotModule => "start_not_module",
    StartNoIdentity => "start_no_consumer_identity",
    StartExisting => "start_existing",
    Started => "started",
    WriterOutboxLock => "writer_outbox_lock_failed",
    WriterConnect => "writer_connect_failed",
    WriterCatalog => "writer_catalog_failed",
    WriterNoDisplay => "writer_no_display",
    WriterAmbiguousDisplay => "writer_ambiguous_display",
    WriterOpenRoute => "writer_open_route_failed",
    WriterDescribe => "writer_describe_failed",
    WriterDescriptionJson => "writer_description_invalid_json",
    WriterDescriptionMissing => "writer_description_missing_limits",
    WriterDescriptionLimits => "writer_description_invalid_limits",
    WriterLimitsLock => "writer_limits_lock_failed",
    WriterTooLarge => "writer_store_limit",
    WriterRequest => "writer_request_failed",
    WriterModuleWarming => "writer_module_warming",
    WriterModuleRefused => "writer_module_refused",
    WriterReplyJson => "writer_reply_invalid_json",
    WriterReplyUnknown => "writer_reply_unknown",
    WriterReplyRefused => "writer_reply_refused",
    Stored => "stored",
    AlreadyStored => "already_stored",
    WriterCloseRoute => "writer_close_route_failed",
    WriterStopped => "writer_stopped",
}

pub(in crate::subc::rich_ui) fn count(counter: Counter) {
    COUNTS[counter as usize].fetch_add(1, Ordering::Relaxed);
}
