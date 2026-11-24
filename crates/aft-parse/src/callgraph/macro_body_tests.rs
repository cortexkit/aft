use super::*;
use crate::calls::RustMacroBody;
use std::cell::Cell;

thread_local! {
    pub(super) static SPAN_CHECKS: Cell<usize> = const { Cell::new(0) };
}

#[test]
fn review_gpt8_disjoint_macro_arms_do_not_cross_scan() {
    const COUNT: usize = 512;
    let source = " ".repeat(COUNT * 20);
    let mut bodies = Vec::new();
    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    for i in 0..COUNT {
        let base = i * 20;
        bodies.push(RustMacroBody {
            name: format!("m{i}"),
            byte_start: base,
            byte_end: base + 12,
        });
        symbols.push(Symbol {
            name: format!("f{i}"),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 0,
                start_col: (base + 2) as u32,
                end_line: 0,
                end_col: (base + 6) as u32,
            },
            signature: None,
            scope_chain: Vec::new(),
            exported: false,
            parent: None,
        });
        for (name, offset) in [("item", 3), ("template", 8), ("ordinary", 15)] {
            calls.push((
                name.into(),
                name.into(),
                1,
                base + offset,
                base + offset + 1,
            ));
        }
    }
    let (ordinary, templates) =
        split_macro_body_calls(&SourceLineIndex::new(&source), &symbols, calls, &bodies);
    assert_eq!(ordinary.len(), 2 * COUNT);
    assert_eq!(templates.len(), COUNT);
    for i in 0..COUNT {
        assert_eq!(ordinary[2 * i].1, "item");
        assert_eq!(ordinary[2 * i + 1].1, "ordinary");
        assert_eq!(templates[&format!("m{i}!")][0].callee_name, "template");
    }
    let checks = SPAN_CHECKS.with(Cell::get);
    eprintln!("{COUNT} disjoint arms, {checks} span predicates");
    assert!(checks < 32 * COUNT, "performed {checks} span predicates");
}

#[test]
fn review_gpt8_sweep_preserves_nested_arm_ties_and_original_call_order() {
    let arms = [(100, 200), (120, 150), (110, 180), (120, 150), (149, 160)];
    let items = [(120, 130), (114, 138), (140, 151), (151, 152)];
    let calls = [
        (152, 154),
        (125, 126),
        (124, 151),
        (135, 137),
        (117, 118),
        (99, 100),
        (198, 199),
        (153, 160),
    ];
    let reference = calls.map(|(start, end)| {
        let (index, &(low, high)) = arms
            .iter()
            .enumerate()
            .filter(|(_, (low, high))| *low <= start && end <= *high)
            .min_by_key(|(_, (low, high))| high - low)?;
        let in_item = items
            .iter()
            .any(|&(left, right)| low <= left && left <= start && end <= right && right <= high);
        (!in_item).then_some(index)
    });
    assert_eq!(
        reference[3],
        Some(1),
        "equal-width arms must retain extraction order"
    );
    let raw = calls.map(|(start, end)| ("call".into(), "call".into(), 1, start, end));
    assert_eq!(macro_body::owners(&raw, &arms, &items), reference);
}
