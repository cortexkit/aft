//! Small FIFO cache of the declaration facts a cross-file call needs.

use std::collections::{HashMap, VecDeque};

const TARGET_BYTES: usize = 128 << 20;
const TARGET_FILES: usize = 4096;

pub(crate) struct TargetCache<T> {
    files: HashMap<String, (usize, T)>,
    oldest: VecDeque<String>,
    bytes: usize,
}

impl<T> Default for TargetCache<T> {
    fn default() -> Self {
        Self {
            files: HashMap::new(),
            oldest: VecDeque::new(),
            bytes: 0,
        }
    }
}

impl<T> TargetCache<T> {
    pub(crate) fn read<R>(
        &mut self,
        path: &str,
        load: impl FnOnce() -> Option<(usize, T)>,
        use_value: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        if let Some((_, value)) = self.files.get(path) {
            return Some(use_value(value));
        }
        let (bytes, value) = load()?;
        if bytes > TARGET_BYTES {
            return Some(use_value(&value));
        }
        while self.bytes + bytes > TARGET_BYTES || self.files.len() >= TARGET_FILES {
            let oldest = self.oldest.pop_front().expect("a full cache has files");
            self.bytes -= self
                .files
                .remove(&oldest)
                .expect("the oldest file is cached")
                .0;
        }
        let result = use_value(&value);
        self.files.insert(path.to_owned(), (bytes, value));
        self.oldest.push_back(path.to_owned());
        self.bytes += bytes;
        Some(result)
    }

    pub(crate) fn held(&self) -> (usize, usize) {
        (self.files.len(), self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_dead_code_target_cache_caps_bytes_and_files() {
        let mut cache = TargetCache::default();
        for file in 0..(TARGET_FILES * 2) {
            assert_eq!(
                cache.read(&file.to_string(), || Some((0, file)), |value| *value),
                Some(file)
            );
        }
        assert_eq!(cache.held(), (TARGET_FILES, 0));

        for path in ["large", "larger"] {
            cache.read(
                path,
                || Some((TARGET_BYTES, 1)),
                |value| assert_eq!(*value, 1),
            );
        }
        assert_eq!(cache.held(), (1, TARGET_BYTES));
        cache.read(
            "oversize",
            || Some((TARGET_BYTES + 1, 2)),
            |value| assert_eq!(*value, 2),
        );
        assert_eq!(cache.held(), (1, TARGET_BYTES));
    }
}
