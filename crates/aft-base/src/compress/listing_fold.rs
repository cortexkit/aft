//! Shared outlier-preserving run folding for `ls` and `find` output.

pub const FOLD_THRESHOLD: usize = 8;
/// Line ceiling for folded listings. Distinct names are kept verbatim below this;
/// middle-cap with an explicit note is only used above this as a last resort.
pub const MAX_LINES: usize = 400;

/// Mask digit runs in `name` to `#` for shape grouping.
pub fn mask_digits_in_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            while chars.peek().is_some_and(|p| p.is_ascii_digit()) {
                chars.next();
            }
            out.push('#');
        } else {
            out.push(c);
        }
    }
    out
}

/// Shape key: `directory|masked_basename` (directory may be empty for plain names).
pub fn shape_key_for_basename(dir: &str, basename: &str) -> String {
    let masked = mask_digits_in_name(basename);
    if dir.is_empty() {
        masked
    } else {
        format!("{dir}|{masked}")
    }
}

/// Display pattern: masked name with `#` → `*`.
pub fn shape_pattern(masked_basename: &str) -> String {
    masked_basename.replace('#', "*")
}

#[derive(Clone, Debug)]
pub struct FoldEntry {
    pub line: String,
    pub dir: String,
    pub basename: String,
    pub shape_key: String,
}

pub fn fold_consecutive_runs(entries: Vec<FoldEntry>) -> Vec<String> {
    if entries.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let key = entries[i].shape_key.clone();
        let mut j = i + 1;
        while j < entries.len() && entries[j].shape_key == key {
            j += 1;
        }
        let run = &entries[i..j];
        if run.len() >= FOLD_THRESHOLD {
            let masked = mask_digits_in_name(&run[0].basename);
            let pattern = if run[0].dir.is_empty() {
                shape_pattern(&masked)
            } else {
                format!(
                    "{}/{}",
                    run[0].dir.trim_end_matches('/'),
                    shape_pattern(&masked)
                )
            };
            let first = display_name(&run[0]);
            let last = display_name(run.last().expect("non-empty run"));
            let count = run.len();
            let noun = if count == 1 { "file" } else { "files" };
            out.push(format!("{pattern} — {count} {noun} ({first} … {last})"));
        } else {
            for e in run {
                out.push(e.line.clone());
            }
        }
        i = j;
    }
    out
}

fn display_name(e: &FoldEntry) -> String {
    if e.dir.is_empty() {
        e.basename.clone()
    } else {
        format!("{}/{}", e.dir.trim_end_matches('/'), e.basename)
    }
}

/// Fold bulk runs, keep every distinct name; only middle-cap as an absolute last
/// resort when line count exceeds [`MAX_LINES`], and say so explicitly.
pub fn finish_folded(lines: Vec<String>) -> String {
    if lines.len() <= MAX_LINES {
        return lines.join("\n");
    }
    let omitted = lines.len() - (MAX_LINES - 1);
    let head_count = (MAX_LINES - 1) / 2;
    let tail_count = (MAX_LINES - 1) - head_count;
    let mut kept: Vec<String> = lines.iter().take(head_count).cloned().collect();
    kept.push(format!(
        "… +{omitted} entries omitted (listing too long; narrow with a path/glob)"
    ));
    kept.extend(lines.iter().skip(lines.len() - tail_count).cloned());
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_digit_runs_in_filename() {
        assert_eq!(mask_digits_in_name("module_017.ts"), "module_#.ts");
        assert_eq!(
            mask_digits_in_name("module_100_NEEDLE_FILE_marker.ts"),
            "module_#_NEEDLE_FILE_marker.ts"
        );
        assert_ne!(
            shape_key_for_basename("", "module_100.ts"),
            shape_key_for_basename("", "module_100_NEEDLE_FILE_marker.ts")
        );
        assert_eq!(
            shape_pattern(&mask_digits_in_name("module_100_NEEDLE_FILE_marker.ts")),
            "module_*_NEEDLE_FILE_marker.ts"
        );
    }
}
