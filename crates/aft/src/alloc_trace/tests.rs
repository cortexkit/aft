//! Unit tests of the tracer's bookkeeping. The library's test binary has its
//! own counting allocator, so these drive the hooks directly; the tests with
//! the tracer installed live with the `aft` binary.

use super::*;

const MIB: isize = 1 << 20;

#[test]
fn tool_labels_are_interned_once_and_odd_names_share_one_label() {
    let first = Label::tool("unit_grep");
    assert_eq!(first, Label::tool("unit_grep"));
    assert_eq!(first.name(), "tool:unit_grep");
    assert_ne!(first, Label::tool("unit_read"));
    assert_eq!(Label::tool("bad name\n"), Label::TOOL_OTHER);
    assert_eq!(Label::tool(""), Label::TOOL_OTHER);
    assert_eq!(Label::custom("index_build"), Label::INDEX_BUILD);
}

#[test]
fn root_name_keeps_the_last_component_cut_at_a_character_boundary() {
    let root = RootName::from_path(Path::new("/work/projects/aft"));
    assert_eq!(root.as_str(), "aft");
    let long = format!("/w/{}é", "x".repeat(46));
    assert_eq!(
        RootName::from_path(Path::new(&long)).as_str(),
        "x".repeat(46)
    );
}

// One test, because each census read restarts every label's mark.
#[test]
fn census_peaks_report_the_high_water_since_the_last_census_highest_first() {
    let find = |peaks: &[LabelPeak], name: &str| {
        peaks
            .iter()
            .position(|peak| peak.label == name)
            .map(|index| (index, peaks[index].clone()))
    };
    let label = Label::custom("unit_census_peak");
    let _scope = scope(label, None);
    account(300 * MIB);
    account(-200 * MIB);
    let peaks = take_census_peaks(MAX_LABELS);
    let (_, peak) = find(&peaks, "unit_census_peak").expect("label reported");
    assert_eq!(peak.peak_bytes, 300 << 20);
    assert_eq!(peak.live_bytes, 100 << 20);
    // The next census starts from what is live now.
    let peaks = take_census_peaks(MAX_LABELS);
    let (_, peak) = find(&peaks, "unit_census_peak").expect("label still live");
    assert_eq!(peak.peak_bytes, 100 << 20);

    // Highest first, and cut at the limit.
    let bigger = Label::custom("unit_census_bigger");
    let _inner = scope(bigger, None);
    account(250 * MIB);
    let peaks = take_census_peaks(MAX_LABELS);
    let (bigger_at, _) = find(&peaks, "unit_census_bigger").unwrap();
    let (smaller_at, _) = find(&peaks, "unit_census_peak").unwrap();
    assert!(bigger_at < smaller_at, "{peaks:?}");
    assert!(take_census_peaks(1).len() <= 1);
}

#[test]
fn a_label_with_no_open_scope_starts_again_from_zero() {
    let label = Label::custom("unit_idle_reset");
    {
        let _scope = scope(label, None);
        account(40 * MIB);
        assert_eq!(live_bytes(label), 40 * MIB);
    }
    // The 40 MiB were handed on when the scope closed.
    assert_eq!(live_bytes(label), 0);
}

#[test]
fn burst_crossing_is_recorded_once_until_it_falls_below_half() {
    let label = Label::custom("unit_burst");
    let state = &LABELS[usize::from(label.0)];
    let _scope = scope(label, None);
    account(LABEL_BURST_BYTES + MIB);
    assert!(state.crossed.load(Ordering::Relaxed));
    account(MIB * 4);
    account(-(LABEL_BURST_BYTES / 4));
    // Still above half the threshold: the same burst.
    assert!(state.crossed.load(Ordering::Relaxed));
    account(-(LABEL_BURST_BYTES / 2));
    assert!(!state.crossed.load(Ordering::Relaxed));
}

#[test]
fn event_lines_name_size_label_thread_and_root() {
    let mut event = ring::Event::new(EventKind::Realloc);
    event.size = 512 << 20;
    event.old_size = 256 << 20;
    event.label = Label::FRAME_ENCODE.0;
    event.root = RootName::from_path(Path::new("/w/aft"));
    event.thread[..16].copy_from_slice(b"aft-subc-runtime");
    event.thread_len = 16;
    assert_eq!(
        report::format_event(&event),
        "large allocation 512 MiB realloc from 256 MiB thread=aft-subc-runtime op=frame_encode root=aft"
    );

    let mut ended = ring::Event::new(EventKind::BurstEnded);
    ended.label = Label::MEANING_FILL.0;
    ended.size = 3 << 30;
    ended.duration_ns = 2_500_000_000;
    assert_eq!(
        report::format_event(&ended),
        "allocation burst ended: op=meaning_fill peak 3072 MiB over 2.5s, live 0.0 MiB now"
    );
}
