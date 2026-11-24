use std::collections::BTreeMap;

use super::{
    compare_directory_code_share, directory_is_data_heavy, directory_rollup_summary,
    files_table_trailer_len_counts, outline_expansion_added_rows, outline_rows_for_directory,
    OutlineDirectoryNode, OutlineFileEntry, OutlineTableRow,
};

pub(super) fn plan(
    roots: &[usize],
    nodes: &[OutlineDirectoryNode],
    files: &[OutlineFileEntry],
    max_bytes: usize,
) -> Vec<OutlineTableRow> {
    let mut table = TableMetrics::default();
    let mut pending = BTreeMap::<usize, Vec<usize>>::new();
    let mut arena = Vec::new();
    for row in roots
        .iter()
        .flat_map(|root| outline_rows_for_directory(*root, nodes, files))
    {
        let metrics = RowMetrics::measure(row, nodes, files);
        table.add(&metrics);
        queue_rollup(row, arena.len(), nodes, &mut pending);
        arena.push(PlannedRow {
            row,
            metrics,
            replacement: 0..0,
        });
    }
    let initial = arena.len();

    // At each depth the cheapest expansions get first use of the budget.
    // Retain stable row positions in the arena rather than cloning or shifting
    // the visible table for each candidate. Flatten the accepted tree at the end.
    while let Some((_, mut candidates)) = pending.pop_first() {
        candidates.sort_by(|&a, &b| {
            let a = &nodes[arena[a].rollup()];
            let b = &nodes[arena[b].rollup()];
            outline_expansion_added_rows(a)
                .cmp(&outline_expansion_added_rows(b))
                .then_with(|| compare_directory_code_share(a, b).reverse())
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut level_fully_expanded = true;
        for position in candidates {
            let replacement = outline_rows_for_directory(arena[position].rollup(), nodes, files);
            if replacement.is_empty() {
                continue;
            }
            table.remove(&arena[position].metrics);
            let start = arena.len();
            for row in replacement {
                let metrics = RowMetrics::measure(row, nodes, files);
                table.add(&metrics);
                arena.push(PlannedRow {
                    row,
                    metrics,
                    replacement: 0..0,
                });
            }
            if table.byte_len() <= max_bytes {
                arena[position].replacement = start..arena.len();
                for index in start..arena.len() {
                    queue_rollup(arena[index].row, index, nodes, &mut pending);
                }
            } else {
                for row in arena.drain(start..) {
                    table.remove(&row.metrics);
                }
                table.add(&arena[position].metrics);
                level_fully_expanded = false;
            }
        }
        if !level_fully_expanded {
            break;
        }
    }

    let mut stack: Vec<_> = (0..initial).rev().collect();
    let mut rows = Vec::with_capacity(table.rows);
    while let Some(position) = stack.pop() {
        let row = &arena[position];
        if row.replacement.is_empty() {
            rows.push(row.row);
        } else {
            stack.extend(row.replacement.clone().rev());
        }
    }
    rows
}

fn queue_rollup(
    row: OutlineTableRow,
    position: usize,
    nodes: &[OutlineDirectoryNode],
    pending: &mut BTreeMap<usize, Vec<usize>>,
) {
    if let OutlineTableRow::Rollup(node) = row {
        if !directory_is_data_heavy(&nodes[node]) {
            pending.entry(nodes[node].depth).or_default().push(position);
        }
    }
}

struct PlannedRow {
    row: OutlineTableRow,
    metrics: RowMetrics,
    replacement: std::ops::Range<usize>,
}

impl PlannedRow {
    fn rollup(&self) -> usize {
        match self.row {
            OutlineTableRow::Rollup(node) => node,
            OutlineTableRow::File(_) => unreachable!("only rollups enter the expansion queue"),
        }
    }
}

struct RowMetrics {
    path_width: usize,
    /// Bytes beyond the padded path/middle columns, including UTF-8 overhead.
    fixed_bytes: usize,
    middle: Middle,
}

enum Middle {
    File {
        language_width: usize,
        symbol_extra: usize,
    },
    Rollup {
        width: usize,
        budget_files: Option<usize>,
    },
}

impl RowMetrics {
    fn measure(
        row: OutlineTableRow,
        nodes: &[OutlineDirectoryNode],
        files: &[OutlineFileEntry],
    ) -> Self {
        #[cfg(test)]
        super::PLANNER_ROW_MEASUREMENTS.with(|count| count.set(count.get() + 1));
        let (path_width, extra, line_width, middle) = match row {
            OutlineTableRow::File(file) => {
                let file = &files[file];
                (
                    file.path.len(),
                    utf8_extra(&file.path) + utf8_extra(&file.language),
                    file.lines.map_or(7, digits).max(7),
                    Middle::File {
                        language_width: file.language.len(),
                        symbol_extra: digits(file.symbols.unwrap_or(0)).saturating_sub(5),
                    },
                )
            }
            OutlineTableRow::Rollup(node) => {
                let node = &nodes[node];
                (
                    node.path.len() + 1,
                    utf8_extra(&node.path),
                    if node.stats.unknown_lines == 0 {
                        digits(node.stats.lines).max(7)
                    } else {
                        7
                    },
                    Middle::Rollup {
                        width: directory_rollup_summary(&node.stats).len(),
                        budget_files: (!directory_is_data_heavy(node)).then_some(node.stats.files),
                    },
                )
            }
        };
        Self {
            path_width,
            fixed_bytes: extra + line_width + 10,
            middle,
        }
    }
}

fn digits(value: usize) -> usize {
    value.checked_ilog10().unwrap_or(0) as usize + 1
}

fn utf8_extra(text: &str) -> usize {
    text.len() - text.chars().count()
}

#[derive(Default)]
struct TableMetrics {
    rows: usize,
    fixed_bytes: usize,
    paths: WidthCounts,
    languages: WidthCounts,
    rollup_summaries: WidthCounts,
    long_symbol_counts: WidthCounts,
    files: usize,
    budget_rollups: usize,
    budget_files: usize,
}

impl TableMetrics {
    fn add(&mut self, row: &RowMetrics) {
        self.update(row, true);
    }

    fn remove(&mut self, row: &RowMetrics) {
        self.update(row, false);
    }

    fn update(&mut self, row: &RowMetrics, add: bool) {
        let change = |value: &mut usize, amount: usize| {
            if add {
                *value += amount;
            } else {
                *value -= amount;
            }
        };
        change(&mut self.rows, 1);
        change(&mut self.fixed_bytes, row.fixed_bytes);
        self.paths.update(row.path_width, add);
        match row.middle {
            Middle::File {
                language_width,
                symbol_extra,
            } => {
                self.languages.update(language_width, add);
                if symbol_extra > 0 {
                    self.long_symbol_counts.update(symbol_extra, add);
                }
                change(&mut self.files, 1);
            }
            Middle::Rollup {
                width,
                budget_files,
            } => {
                self.rollup_summaries.update(width, add);
                if let Some(files) = budget_files {
                    change(&mut self.budget_rollups, 1);
                    change(&mut self.budget_files, files);
                }
            }
        }
    }

    fn byte_len(&self) -> usize {
        let file_middle_width = self.languages.max().max(8) + 11;
        let middle_width = self.rollup_summaries.max().max(file_middle_width);
        // A symbol count wider than five digits can spill past the padding.
        // Only the distinct digit widths matter, not the number of file rows.
        let spare = middle_width - file_middle_width;
        let spill: usize = self
            .long_symbol_counts
            .0
            .iter()
            .map(|(extra, rows)| extra.saturating_sub(spare) * rows)
            .sum();
        self.rows * (self.paths.max() + middle_width)
            + self.fixed_bytes
            + spill
            + files_table_trailer_len_counts(
                self.files,
                self.budget_files,
                self.budget_rollups != 0,
            )
    }
}

#[derive(Default)]
struct WidthCounts(BTreeMap<usize, usize>);

impl WidthCounts {
    fn max(&self) -> usize {
        self.0.last_key_value().map_or(0, |(&width, _)| width)
    }

    fn update(&mut self, width: usize, add: bool) {
        if add {
            *self.0.entry(width).or_default() += 1;
        } else if let std::collections::btree_map::Entry::Occupied(mut entry) = self.0.entry(width)
        {
            *entry.get_mut() -= 1;
            if *entry.get() == 0 {
                entry.remove();
            }
        }
    }
}
