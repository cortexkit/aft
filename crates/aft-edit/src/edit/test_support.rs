use super::DiffBudget;

pub mod bash {
    pub use super::super::bash::{
        find_bash, reported_line, run_bash, with_runner, CheckOutcome, Runner, CHECK_TIMEOUT,
        SYNTAX_ERROR_STATUS,
    };
}

pub mod swift {
    #[cfg(unix)]
    pub use super::super::swift::run_swiftc_for_test;
    pub use super::super::swift::{
        cached_lookup, parse_outcome, parse_with_lookup, run_command, with_runner, LookupOutcome,
        ParseOutcome, Runner, BROKEN, TUPLE,
    };
}

pub fn diff_budget_remaining(budget: &DiffBudget) -> usize {
    budget.remaining
}

pub fn set_diff_budget_remaining(budget: &mut DiffBudget, remaining: usize) {
    budget.remaining = remaining;
}

pub fn diff_budget_omitted(budget: &DiffBudget) -> bool {
    budget.omitted
}

pub struct BoundedDiffText(super::BoundedDiffText);

impl BoundedDiffText {
    pub fn new(limit: usize) -> Self {
        Self(super::BoundedDiffText {
            text: String::new(),
            limit,
        })
    }

    pub fn text(&self) -> &String {
        &self.0.text
    }
}

impl std::fmt::Write for BoundedDiffText {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        std::fmt::Write::write_str(&mut self.0, text)
    }
}
