use std::collections::BTreeMap;

pub const CAP_ERRORS: usize = 20;
pub const CAP_WARNINGS: usize = 10;
pub const CAP_LIST: usize = 20;
pub const CAP_INVENTORY: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropClass {
    Error,
    Warning,
    Failure,
    Issue,
    List,
    Inventory,
    Timing,
}

impl DropClass {
    pub fn default_cap(self) -> usize {
        match self {
            Self::Error | Self::Failure => CAP_ERRORS,
            Self::Warning => CAP_WARNINGS,
            Self::Issue | Self::List | Self::Timing => CAP_LIST,
            Self::Inventory => CAP_INVENTORY,
        }
    }

    pub fn singular(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Failure => "failure",
            Self::Issue => "issue",
            Self::List => "list item",
            Self::Inventory => "inventory item",
            Self::Timing => "timing line",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Self::Error => "errors",
            Self::Warning => "warnings",
            Self::Failure => "failures",
            Self::Issue => "issues",
            Self::List => "list items",
            Self::Inventory => "inventory items",
            Self::Timing => "timing lines",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedBlock {
    pub class: Option<DropClass>,
    pub text: String,
}

impl ClassifiedBlock {
    pub fn new(class: DropClass, text: impl Into<String>) -> Self {
        Self {
            class: Some(class),
            text: text.into(),
        }
    }

    pub fn unclassified(text: impl Into<String>) -> Self {
        Self {
            class: None,
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassCapResult {
    pub text: String,
    pub dropped_by_class: BTreeMap<DropClass, usize>,
}

pub fn cap_classified_blocks(blocks: Vec<ClassifiedBlock>) -> ClassCapResult {
    cap_classified_blocks_with(blocks, DropClass::default_cap)
}

pub fn cap_classified_blocks_with<F>(blocks: Vec<ClassifiedBlock>, cap_for: F) -> ClassCapResult
where
    F: Fn(DropClass) -> usize,
{
    let mut seen_by_class: BTreeMap<DropClass, usize> = BTreeMap::new();
    let mut dropped_by_class: BTreeMap<DropClass, usize> = BTreeMap::new();
    let mut kept = Vec::new();

    for block in blocks {
        let Some(class) = block.class else {
            kept.push(block.text);
            continue;
        };

        let seen = seen_by_class.entry(class).or_default();
        *seen += 1;
        if *seen <= cap_for(class) {
            kept.push(block.text);
        } else {
            *dropped_by_class.entry(class).or_default() += 1;
        }
    }

    ClassCapResult {
        text: join_blocks(kept),
        dropped_by_class,
    }
}

pub fn join_blocks(blocks: Vec<String>) -> String {
    blocks
        .into_iter()
        .map(|block| block.trim_end().to_string())
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_error_and_warning_blocks_spread_through_long_stream() {
        let mut blocks = Vec::new();
        for index in 0..40 {
            blocks.push(ClassifiedBlock::new(
                DropClass::Error,
                format!("error {index}\n  error context {index}"),
            ));
            if index < 20 {
                blocks.push(ClassifiedBlock::new(
                    DropClass::Warning,
                    format!("warning {index}\n  warning context {index}"),
                ));
            }
            blocks.push(ClassifiedBlock::unclassified(format!("progress {index}")));
        }

        let capped = cap_classified_blocks(blocks);

        assert_eq!(capped.text.matches("error context").count(), CAP_ERRORS);
        assert_eq!(capped.text.matches("warning context").count(), CAP_WARNINGS);
        assert_eq!(capped.dropped_by_class.get(&DropClass::Error), Some(&20));
        assert_eq!(capped.dropped_by_class.get(&DropClass::Warning), Some(&10));
        assert!(capped.text.contains("progress 39"));
        assert!(!capped.text.contains("error 39\n  error context 39"));
    }

    #[test]
    fn caps_by_class_without_splitting_blocks() {
        let blocks = vec![
            ClassifiedBlock::new(DropClass::Error, "error 1\n  context"),
            ClassifiedBlock::new(DropClass::Warning, "warning 1\n  context"),
            ClassifiedBlock::new(DropClass::Error, "error 2\n  context"),
        ];

        let capped = cap_classified_blocks_with(blocks, |class| match class {
            DropClass::Error => 1,
            DropClass::Warning => 10,
            _ => 0,
        });

        assert!(capped.text.contains("error 1\n  context"));
        assert!(capped.text.contains("warning 1\n  context"));
        assert!(!capped.text.contains("error 2"));
        assert_eq!(capped.dropped_by_class.get(&DropClass::Error), Some(&1));
    }
}
