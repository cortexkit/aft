use aft_tokenizer::count_tokens;
use std::time::{Duration, Instant};

const INPUT_BYTES: usize = 128 * 1024;

fn repeat_to_size(seed: &str) -> String {
    seed.repeat(INPUT_BYTES.div_ceil(seed.len()))[..INPUT_BYTES].to_owned()
}

fn fixtures() -> Vec<(&'static str, String)> {
    let normal = repeat_to_size(include_str!(
        "../../../benchmarks/compression-tokens/fixtures/deploy-container/journalctl.txt"
    ));
    let unbroken = repeat_to_size("abcdefghijklmnopqrstuvwxyz0123456789");
    // Deterministic, varied base64 characters without line wrapping.
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state = 0x1234_5678_u32;
    let base64 = (0..INPUT_BYTES)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            alphabet[(state >> 26) as usize] as char
        })
        .collect();
    let progress = repeat_to_size("\r[========================================]42%1234567890");
    vec![
        ("normal", normal),
        ("unbroken", unbroken),
        ("base64", base64),
        ("progress", progress),
        ("repeated", "a".repeat(INPUT_BYTES)),
    ]
}

#[test]
fn long_run_count_timings() {
    // Initialize the shared vocabulary before measuring per-input work.
    count_tokens("warm up").unwrap();
    for (name, text) in fixtures() {
        for bytes in [16 * 1024, 32 * 1024, 64 * 1024, INPUT_BYTES] {
            let start = Instant::now();
            let count = count_tokens(std::hint::black_box(&text[..bytes])).unwrap();
            let elapsed = start.elapsed();
            eprintln!("{name}: {bytes} bytes, {count} tokens, {elapsed:?}");
            assert!(count > 0);
            if bytes == INPUT_BYTES {
                assert!(
                    elapsed < Duration::from_millis(500),
                    "{name}: counting {bytes} bytes took {elapsed:?}"
                );
                if name == "normal" {
                    // Fixed normal-log baseline, not computed from another
                    // invocation of the tokenizer under test.
                    assert_eq!(count, 40_280);
                }
            }
        }
    }
}
