use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};
use regex_syntax::hir::{Hir, HirKind};

use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::fs_lock;

const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;
const CACHE_MAGIC: u32 = 0x3144_4958; // "XID1" little-endian
const INDEX_MAGIC: &[u8; 8] = b"AFTIDX01";
const LOOKUP_MAGIC: &[u8; 8] = b"AFTLKP01";
const INDEX_VERSION: u32 = 3;
const PREVIEW_BYTES: usize = 8 * 1024;
const EOF_SENTINEL: u8 = 0;
const MAX_ENTRIES: usize = 10_000_000;
const MIN_FILE_ENTRY_BYTES: usize = 57;
const LOOKUP_ENTRY_BYTES: usize = 16;
const POSTING_BYTES: usize = 6;
static CACHE_LOCK_ACQUIRE_MUTEX: Mutex<()> = Mutex::new(());

pub struct CacheLock {
    _guard: fs_lock::LockGuard,
}

impl CacheLock {
    pub fn acquire(cache_dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(cache_dir)?;
        let path = cache_dir.join("cache.lock");
        let _acquire_guard = CACHE_LOCK_ACQUIRE_MUTEX
            .lock()
            .map_err(|_| std::io::Error::other("search cache lock acquisition mutex poisoned"))?;
        fs_lock::try_acquire(&path, Duration::from_secs(2))
            .map(|guard| Self { _guard: guard })
            .map_err(|error| match error {
                fs_lock::AcquireError::Timeout => {
                    std::io::Error::other("timed out acquiring search cache lock")
                }
                fs_lock::AcquireError::Io(error) => error,
            })
    }
}

#[derive(Clone, Debug)]
pub struct SearchIndex {
    pub postings: HashMap<u32, Vec<Posting>>,
    pub files: Vec<FileEntry>,
    pub path_to_id: HashMap<PathBuf, u32>,
    pub ready: bool,
    project_root: PathBuf,
    git_head: Option<String>,
    max_file_size: u64,
    pub file_trigrams: HashMap<u32, Vec<u32>>,
    unindexed_files: HashSet<u32>,
}

impl SearchIndex {
    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Number of unique trigrams in the index.
    pub fn trigram_count(&self) -> usize {
        self.postings.len()
    }

    /// Compute distinct query trigrams from literal tokens.
    pub fn query_trigrams_from_tokens(tokens: &[&str]) -> Vec<u32> {
        query_trigrams_from_tokens(tokens)
    }

    /// Score-rank file candidates by lexical relevance to query trigrams.
    pub fn lexical_rank(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        max_files: usize,
    ) -> Vec<(PathBuf, f32)> {
        if query_trigrams.is_empty() || max_files == 0 {
            return Vec::new();
        }

        let mut non_zero: Vec<(u32, usize)> = query_trigrams
            .iter()
            .filter_map(|trigram| {
                let posting_count = self.postings.get(trigram).map_or(0, Vec::len);
                (posting_count > 0).then_some((*trigram, posting_count))
            })
            .collect();
        if non_zero.is_empty() {
            return Vec::new();
        }

        non_zero.sort_unstable_by_key(|(_, posting_count)| *posting_count);
        let selected_count = non_zero.len().min(3);
        let candidate_cap = if selected_count == 3 { 200 } else { 500 };

        let mut candidate_ids = BTreeSet::new();
        for (trigram, _) in non_zero.iter().take(selected_count) {
            if let Some(postings) = self.postings.get(trigram) {
                for posting in postings {
                    if self.is_active_file(posting.file_id) {
                        candidate_ids.insert(posting.file_id);
                    }
                }
            }
        }

        let mut ranked = Vec::new();
        for file_id in candidate_ids.into_iter().take(candidate_cap) {
            let Some(entry) = self.files.get(file_id as usize) else {
                continue;
            };
            if let Some(filter) = candidate_filter {
                if !filter(&entry.path) {
                    continue;
                }
            }
            let score = lexical_score(self, query_trigrams, file_id);
            if score > 0.0 {
                ranked.push((entry.path.clone(), score));
            }
        }

        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(max_files);
        ranked
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posting {
    pub file_id: u32,
    pub next_mask: u8,
    pub loc_mask: u8,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    pub content_hash: blake3::Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepMatch {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub line_text: String,
    pub match_text: String,
}

#[derive(Clone, Debug)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub total_matches: usize,
    pub files_searched: usize,
    pub files_with_matches: usize,
    pub index_status: IndexStatus,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexStatus {
    Ready,
    Building,
    Fallback,
}

impl IndexStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            IndexStatus::Ready => "Ready",
            IndexStatus::Building => "Building",
            IndexStatus::Fallback => "Fallback",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RegexQuery {
    pub and_trigrams: Vec<u32>,
    pub or_groups: Vec<Vec<u32>>,
    pub(crate) and_filters: HashMap<u32, PostingFilter>,
    pub(crate) or_filters: Vec<HashMap<u32, PostingFilter>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PostingFilter {
    next_mask: u8,
    loc_mask: u8,
}

#[derive(Clone, Debug, Default)]
struct QueryBuild {
    and_runs: Vec<Vec<u8>>,
    or_groups: Vec<Vec<Vec<u8>>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PathFilters {
    includes: Option<GlobSet>,
    excludes: Option<GlobSet>,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchScope {
    pub root: PathBuf,
    pub use_index: bool,
}

#[derive(Clone, Debug)]
struct SharedGrepMatch {
    file: Arc<PathBuf>,
    line: u32,
    column: u32,
    line_text: String,
    match_text: String,
}

#[derive(Clone, Debug)]
enum SearchMatcher {
    Literal(LiteralSearch),
    Regex(Regex),
}

#[derive(Clone, Debug)]
enum LiteralSearch {
    CaseSensitive(Vec<u8>),
    AsciiCaseInsensitive(Vec<u8>),
}

impl SearchIndex {
    pub fn new() -> Self {
        SearchIndex {
            postings: HashMap::new(),
            files: Vec::new(),
            path_to_id: HashMap::new(),
            ready: false,
            project_root: PathBuf::new(),
            git_head: None,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            file_trigrams: HashMap::new(),
            unindexed_files: HashSet::new(),
        }
    }

    pub fn build(root: &Path) -> Self {
        Self::build_with_limit(root, DEFAULT_MAX_FILE_SIZE)
    }

    pub(crate) fn build_with_limit(root: &Path, max_file_size: u64) -> Self {
        let project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut index = SearchIndex {
            project_root: project_root.clone(),
            max_file_size,
            ..SearchIndex::new()
        };

        let filters = PathFilters::default();
        for path in walk_project_files(&project_root, &filters) {
            index.update_file(&path);
        }

        index.git_head = current_git_head(&project_root);
        index.ready = true;
        index
    }

    pub fn index_file(&mut self, path: &Path, content: &[u8]) {
        self.remove_file(path);

        let file_id = match self.allocate_file_id(path, content.len() as u64) {
            Some(file_id) => file_id,
            None => return,
        };
        if let Some(file) = self.files.get_mut(file_id as usize) {
            file.content_hash = cache_freshness::hash_bytes(content);
        }

        let mut trigram_map: BTreeMap<u32, PostingFilter> = BTreeMap::new();
        for (trigram, next_char, position) in extract_trigrams(content) {
            let entry = trigram_map.entry(trigram).or_default();
            entry.next_mask |= mask_for_next_char(next_char);
            entry.loc_mask |= mask_for_position(position);
        }

        let mut file_trigrams = Vec::with_capacity(trigram_map.len());
        for (trigram, filter) in trigram_map {
            let postings = self.postings.entry(trigram).or_default();
            postings.push(Posting {
                file_id,
                next_mask: filter.next_mask,
                loc_mask: filter.loc_mask,
            });
            // Posting lists are kept sorted by file_id for binary search during
            // intersection. Since file_ids are allocated incrementally, the new
            // entry is usually already in order. Only sort when needed.
            if postings.len() > 1
                && postings[postings.len() - 2].file_id > postings[postings.len() - 1].file_id
            {
                postings.sort_unstable_by_key(|p| p.file_id);
            }
            file_trigrams.push(trigram);
        }

        self.file_trigrams.insert(file_id, file_trigrams);
        self.unindexed_files.remove(&file_id);
    }

    pub fn remove_file(&mut self, path: &Path) {
        let Some(file_id) = self.path_to_id.remove(path) else {
            return;
        };

        if let Some(trigrams) = self.file_trigrams.remove(&file_id) {
            for trigram in trigrams {
                let should_remove = if let Some(postings) = self.postings.get_mut(&trigram) {
                    postings.retain(|posting| posting.file_id != file_id);
                    postings.is_empty()
                } else {
                    false
                };

                if should_remove {
                    self.postings.remove(&trigram);
                }
            }
        }

        self.unindexed_files.remove(&file_id);
        if let Some(file) = self.files.get_mut(file_id as usize) {
            file.path = PathBuf::new();
            file.size = 0;
            file.modified = UNIX_EPOCH;
            file.content_hash = cache_freshness::zero_hash();
        }
    }

    pub fn update_file(&mut self, path: &Path) {
        self.remove_file(path);

        let metadata = match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return,
        };

        if is_binary_path(path, metadata.len()) {
            self.track_unindexed_file(path, &metadata);
            return;
        }

        if metadata.len() > self.max_file_size {
            self.track_unindexed_file(path, &metadata);
            return;
        }

        let content = match fs::read(path) {
            Ok(content) => content,
            Err(_) => return,
        };

        if is_binary_bytes(&content) {
            self.track_unindexed_file(path, &metadata);
            return;
        }

        self.index_file(path, &content);
    }

    pub fn grep(
        &self,
        pattern: &str,
        case_sensitive: bool,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        self.search_grep(
            pattern,
            case_sensitive,
            include,
            exclude,
            search_root,
            max_results,
        )
    }

    pub fn search_grep(
        &self,
        pattern: &str,
        case_sensitive: bool,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        // Detect if pattern is a plain literal (no regex metacharacters).
        // If so, use memchr::memmem which is 3-10x faster than regex for byte scanning.
        let is_literal = !pattern.chars().any(|c| {
            matches!(
                c,
                '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
            )
        });

        let literal_search = if is_literal {
            if case_sensitive {
                Some(LiteralSearch::CaseSensitive(pattern.as_bytes().to_vec()))
            } else if pattern.is_ascii() {
                Some(LiteralSearch::AsciiCaseInsensitive(
                    pattern
                        .as_bytes()
                        .iter()
                        .map(|byte| byte.to_ascii_lowercase())
                        .collect(),
                ))
            } else {
                None
            }
        } else {
            None
        };

        // Build the regex for non-literal patterns (or literal Unicode fallback).
        let regex = if literal_search.is_some() {
            None
        } else {
            let regex_pattern = if is_literal {
                regex::escape(pattern)
            } else {
                pattern.to_string()
            };
            let mut builder = RegexBuilder::new(&regex_pattern);
            builder.case_insensitive(!case_sensitive);
            // Treat `^` and `$` as line anchors (grep semantics), not file anchors.
            builder.multi_line(true);
            match builder.build() {
                Ok(r) => Some(r),
                Err(_) => {
                    return GrepResult {
                        matches: Vec::new(),
                        total_matches: 0,
                        files_searched: 0,
                        files_with_matches: 0,
                        index_status: if self.ready {
                            IndexStatus::Ready
                        } else {
                            IndexStatus::Building
                        },
                        truncated: false,
                    };
                }
            }
        };

        let matcher = if let Some(literal_search) = literal_search {
            SearchMatcher::Literal(literal_search)
        } else {
            SearchMatcher::Regex(
                regex.expect("regex should exist when literal matcher is unavailable"),
            )
        };

        let filters = match build_path_filters(include, exclude) {
            Ok(filters) => filters,
            Err(_) => PathFilters::default(),
        };
        let search_root = canonicalize_or_normalize(search_root);

        let query = decompose_regex(pattern);
        let candidate_ids = self.candidates(&query);

        let candidate_files: Vec<&FileEntry> = candidate_ids
            .into_iter()
            .filter_map(|file_id| self.files.get(file_id as usize))
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter(|file| is_within_search_root(&search_root, &file.path))
            .filter(|file| filters.matches(&self.project_root, &file.path))
            .collect();

        let total_matches = AtomicUsize::new(0);
        let files_searched = AtomicUsize::new(0);
        let files_with_matches = AtomicUsize::new(0);
        let truncated = AtomicBool::new(false);
        let stop_after = max_results.saturating_mul(2);

        let mut matches = if candidate_files.len() > 10 {
            candidate_files
                .par_iter()
                .map(|file| {
                    search_candidate_file(
                        file,
                        &matcher,
                        max_results,
                        stop_after,
                        &total_matches,
                        &files_searched,
                        &files_with_matches,
                        &truncated,
                    )
                })
                .reduce(Vec::new, |mut left, mut right| {
                    left.append(&mut right);
                    left
                })
        } else {
            let mut matches = Vec::new();
            for file in candidate_files {
                matches.extend(search_candidate_file(
                    file,
                    &matcher,
                    max_results,
                    stop_after,
                    &total_matches,
                    &files_searched,
                    &files_with_matches,
                    &truncated,
                ));

                if should_stop_search(&truncated, &total_matches, stop_after) {
                    break;
                }
            }
            matches
        };

        sort_shared_grep_matches_by_cached_mtime_desc(&mut matches, |path| {
            self.path_to_id
                .get(path)
                .and_then(|file_id| self.files.get(*file_id as usize))
                .map(|file| file.modified)
        });

        let matches = matches
            .into_iter()
            .map(|matched| GrepMatch {
                file: matched.file.as_ref().clone(),
                line: matched.line,
                column: matched.column,
                line_text: matched.line_text,
                match_text: matched.match_text,
            })
            .collect();

        GrepResult {
            total_matches: total_matches.load(Ordering::Relaxed),
            matches,
            files_searched: files_searched.load(Ordering::Relaxed),
            files_with_matches: files_with_matches.load(Ordering::Relaxed),
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            truncated: truncated.load(Ordering::Relaxed),
        }
    }

    pub fn glob(&self, pattern: &str, search_root: &Path) -> Vec<PathBuf> {
        let filters = match build_path_filters(&[pattern.to_string()], &[]) {
            Ok(filters) => filters,
            Err(_) => return Vec::new(),
        };
        let search_root = canonicalize_or_normalize(search_root);
        let mut entries = self
            .files
            .iter()
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter(|file| is_within_search_root(&search_root, &file.path))
            .filter(|file| filters.matches(&self.project_root, &file.path))
            .map(|file| (file.path.clone(), file.modified))
            .collect::<Vec<_>>();

        entries.sort_by(|(left_path, left_mtime), (right_path, right_mtime)| {
            right_mtime
                .cmp(left_mtime)
                .then_with(|| left_path.cmp(right_path))
        });

        entries.into_iter().map(|(path, _)| path).collect()
    }

    pub fn candidates(&self, query: &RegexQuery) -> Vec<u32> {
        if query.and_trigrams.is_empty() && query.or_groups.is_empty() {
            return self.active_file_ids();
        }

        let mut and_trigrams = query.and_trigrams.clone();
        and_trigrams.sort_unstable_by_key(|trigram| self.postings.get(trigram).map_or(0, Vec::len));

        let mut current: Option<Vec<u32>> = None;

        for trigram in and_trigrams {
            let filter = query.and_filters.get(&trigram).copied();
            let matches = self.postings_for_trigram(trigram, filter);
            current = Some(match current.take() {
                Some(existing) => intersect_sorted_ids(&existing, &matches),
                None => matches,
            });

            if current.as_ref().is_some_and(|ids| ids.is_empty()) {
                break;
            }
        }

        let mut current = current.unwrap_or_else(|| self.active_file_ids());

        for (index, group) in query.or_groups.iter().enumerate() {
            let mut group_matches = Vec::new();
            let filters = query.or_filters.get(index);

            for trigram in group {
                let filter = filters.and_then(|filters| filters.get(trigram).copied());
                let matches = self.postings_for_trigram(*trigram, filter);
                if group_matches.is_empty() {
                    group_matches = matches;
                } else {
                    group_matches = union_sorted_ids(&group_matches, &matches);
                }
            }

            current = intersect_sorted_ids(&current, &group_matches);
            if current.is_empty() {
                break;
            }
        }

        let mut unindexed = self
            .unindexed_files
            .iter()
            .copied()
            .filter(|file_id| self.is_active_file(*file_id))
            .collect::<Vec<_>>();
        if !unindexed.is_empty() {
            unindexed.sort_unstable();
            current = union_sorted_ids(&current, &unindexed);
        }

        current
    }

    pub fn write_to_disk(&self, cache_dir: &Path, git_head: Option<&str>) {
        if fs::create_dir_all(cache_dir).is_err() {
            return;
        }

        let cache_path = cache_dir.join("cache.bin");
        let tmp_cache = cache_dir.join(format!(
            "cache.bin.tmp.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos()
        ));

        let active_ids = self.active_file_ids();
        let mut id_map = HashMap::new();
        for (new_id, old_id) in active_ids.iter().enumerate() {
            let Ok(new_id_u32) = u32::try_from(new_id) else {
                return;
            };
            id_map.insert(*old_id, new_id_u32);
        }

        let write_result = (|| -> std::io::Result<()> {
            let mut postings_writer = BufWriter::new(Cursor::new(Vec::new()));

            postings_writer.write_all(INDEX_MAGIC)?;
            write_u32(&mut postings_writer, INDEX_VERSION)?;

            let head = git_head.unwrap_or_default();
            let root = self.project_root.to_string_lossy();
            let head_len = u32::try_from(head.len())
                .map_err(|_| std::io::Error::other("git head too large to cache"))?;
            let root_len = u32::try_from(root.len())
                .map_err(|_| std::io::Error::other("project root too large to cache"))?;
            let file_count = u32::try_from(active_ids.len())
                .map_err(|_| std::io::Error::other("too many files to cache"))?;

            write_u32(&mut postings_writer, head_len)?;
            write_u32(&mut postings_writer, root_len)?;
            write_u64(&mut postings_writer, self.max_file_size)?;
            write_u32(&mut postings_writer, file_count)?;
            postings_writer.write_all(head.as_bytes())?;
            postings_writer.write_all(root.as_bytes())?;

            for old_id in &active_ids {
                let Some(file) = self.files.get(*old_id as usize) else {
                    return Err(std::io::Error::other("missing file entry for cache write"));
                };
                let path =
                    cache_relative_path(&self.project_root, &file.path).ok_or_else(|| {
                        std::io::Error::other(format!(
                            "refusing to cache path outside project root: {}",
                            file.path.display()
                        ))
                    })?;
                let path = path.to_string_lossy();
                let path_len = u32::try_from(path.len())
                    .map_err(|_| std::io::Error::other("cached path too large"))?;
                let modified = file
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO);
                let unindexed = if self.unindexed_files.contains(old_id) {
                    1u8
                } else {
                    0u8
                };

                postings_writer.write_all(&[unindexed])?;
                write_u32(&mut postings_writer, path_len)?;
                write_u64(&mut postings_writer, file.size)?;
                write_u64(&mut postings_writer, modified.as_secs())?;
                write_u32(&mut postings_writer, modified.subsec_nanos())?;
                postings_writer.write_all(file.content_hash.as_bytes())?;
                postings_writer.write_all(path.as_bytes())?;
            }

            let mut lookup_entries = Vec::new();
            let mut postings_blob = Vec::new();
            let mut sorted_postings: Vec<_> = self.postings.iter().collect();
            sorted_postings.sort_by_key(|(trigram, _)| **trigram);

            for (trigram, postings) in sorted_postings {
                let offset = u64::try_from(postings_blob.len())
                    .map_err(|_| std::io::Error::other("postings blob too large"))?;
                let mut count = 0u32;

                for posting in postings {
                    let Some(mapped_file_id) = id_map.get(&posting.file_id).copied() else {
                        continue;
                    };

                    postings_blob.extend_from_slice(&mapped_file_id.to_le_bytes());
                    postings_blob.push(posting.next_mask);
                    postings_blob.push(posting.loc_mask);
                    count = count.saturating_add(1);
                }

                if count > 0 {
                    lookup_entries.push((*trigram, offset, count));
                }
            }

            write_u64(
                &mut postings_writer,
                u64::try_from(postings_blob.len())
                    .map_err(|_| std::io::Error::other("postings blob too large"))?,
            )?;
            postings_writer.write_all(&postings_blob)?;
            postings_writer.flush()?;
            let mut postings_blob_file = postings_writer
                .into_inner()
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .into_inner();
            let checksum = crc32fast::hash(&postings_blob_file);
            postings_blob_file.extend_from_slice(&checksum.to_le_bytes());

            let mut lookup_writer = BufWriter::new(Cursor::new(Vec::new()));
            let entry_count = u32::try_from(lookup_entries.len())
                .map_err(|_| std::io::Error::other("too many lookup entries to cache"))?;

            lookup_writer.write_all(LOOKUP_MAGIC)?;
            write_u32(&mut lookup_writer, INDEX_VERSION)?;
            write_u32(&mut lookup_writer, entry_count)?;

            for (trigram, offset, count) in lookup_entries {
                write_u32(&mut lookup_writer, trigram)?;
                write_u64(&mut lookup_writer, offset)?;
                write_u32(&mut lookup_writer, count)?;
            }

            lookup_writer.flush()?;
            let mut lookup_blob_file = lookup_writer
                .into_inner()
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .into_inner();
            let checksum = crc32fast::hash(&lookup_blob_file);
            lookup_blob_file.extend_from_slice(&checksum.to_le_bytes());

            let mut cache_writer = BufWriter::new(File::create(&tmp_cache)?);
            write_u32(&mut cache_writer, CACHE_MAGIC)?;
            write_u32(&mut cache_writer, INDEX_VERSION)?;
            write_u64(
                &mut cache_writer,
                u64::try_from(postings_blob_file.len())
                    .map_err(|_| std::io::Error::other("postings section too large"))?,
            )?;
            cache_writer.write_all(&postings_blob_file)?;
            cache_writer.write_all(&lookup_blob_file)?;
            cache_writer.flush()?;
            cache_writer.get_ref().sync_all()?;
            drop(cache_writer);
            fs::rename(&tmp_cache, &cache_path)?;

            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&tmp_cache);
        }
    }

    pub fn read_from_disk(cache_dir: &Path, current_canonical_root: &Path) -> Option<Self> {
        debug_assert!(current_canonical_root.is_absolute());
        let cache_path = cache_dir.join("cache.bin");
        let cache_bytes = fs::read(&cache_path).ok()?;
        if cache_bytes.len() < 16 {
            return None;
        }
        let mut header = Cursor::new(&cache_bytes);
        if read_u32(&mut header).ok()? != CACHE_MAGIC {
            return None;
        }
        if read_u32(&mut header).ok()? != INDEX_VERSION {
            return None;
        }
        let postings_len_total = usize::try_from(read_u64(&mut header).ok()?).ok()?;
        let start = usize::try_from(header.position()).ok()?;
        let postings_end = start.checked_add(postings_len_total)?;
        if postings_end > cache_bytes.len() {
            return None;
        }
        let postings_bytes = &cache_bytes[start..postings_end];
        let lookup_bytes = &cache_bytes[postings_end..];
        let lookup_len_total = lookup_bytes.len();
        let mut postings_reader = BufReader::new(Cursor::new(postings_bytes));
        let mut lookup_reader = BufReader::new(Cursor::new(lookup_bytes));
        if postings_len_total < 4 || lookup_len_total < 4 {
            return None;
        }
        verify_crc32_bytes_slice(postings_bytes).ok()?;
        verify_crc32_bytes_slice(lookup_bytes).ok()?;

        let mut magic = [0u8; 8];
        postings_reader.read_exact(&mut magic).ok()?;
        if &magic != INDEX_MAGIC {
            return None;
        }
        if read_u32(&mut postings_reader).ok()? != INDEX_VERSION {
            return None;
        }

        let head_len = read_u32(&mut postings_reader).ok()? as usize;
        let root_len = read_u32(&mut postings_reader).ok()? as usize;
        let max_file_size = read_u64(&mut postings_reader).ok()?;
        let file_count = read_u32(&mut postings_reader).ok()? as usize;
        if file_count > MAX_ENTRIES {
            return None;
        }
        let postings_body_len = postings_len_total.checked_sub(4)?;
        let lookup_body_len = lookup_len_total.checked_sub(4)?;

        let remaining_postings = remaining_bytes(&mut postings_reader, postings_body_len)?;
        let minimum_file_bytes = file_count.checked_mul(MIN_FILE_ENTRY_BYTES)?;
        if minimum_file_bytes > remaining_postings {
            return None;
        }

        if head_len > remaining_bytes(&mut postings_reader, postings_body_len)? {
            return None;
        }
        let mut head_bytes = vec![0u8; head_len];
        postings_reader.read_exact(&mut head_bytes).ok()?;
        let git_head = String::from_utf8(head_bytes)
            .ok()
            .filter(|head| !head.is_empty());

        if root_len > remaining_bytes(&mut postings_reader, postings_body_len)? {
            return None;
        }
        let mut root_bytes = vec![0u8; root_len];
        postings_reader.read_exact(&mut root_bytes).ok()?;
        let _stored_project_root = PathBuf::from(String::from_utf8(root_bytes).ok()?);
        let project_root = current_canonical_root.to_path_buf();

        let mut files = Vec::with_capacity(file_count);
        let mut path_to_id = HashMap::new();
        let mut unindexed_files = HashSet::new();

        for file_id in 0..file_count {
            let mut unindexed = [0u8; 1];
            postings_reader.read_exact(&mut unindexed).ok()?;
            let path_len = read_u32(&mut postings_reader).ok()? as usize;
            let size = read_u64(&mut postings_reader).ok()?;
            let secs = read_u64(&mut postings_reader).ok()?;
            let nanos = read_u32(&mut postings_reader).ok()?;
            let mut hash_bytes = [0u8; 32];
            postings_reader.read_exact(&mut hash_bytes).ok()?;
            let content_hash = blake3::Hash::from_bytes(hash_bytes);
            if nanos >= 1_000_000_000 {
                return None;
            }
            if path_len > remaining_bytes(&mut postings_reader, postings_body_len)? {
                return None;
            }
            let mut path_bytes = vec![0u8; path_len];
            postings_reader.read_exact(&mut path_bytes).ok()?;
            let relative_path = PathBuf::from(String::from_utf8(path_bytes).ok()?);
            let full_path = cached_path_under_root(&project_root, &relative_path)?;
            let file_id_u32 = u32::try_from(file_id).ok()?;

            files.push(FileEntry {
                path: full_path.clone(),
                size,
                modified: UNIX_EPOCH + Duration::new(secs, nanos),
                content_hash,
            });
            path_to_id.insert(full_path, file_id_u32);
            if unindexed[0] == 1 {
                unindexed_files.insert(file_id_u32);
            }
        }

        let postings_len = read_u64(&mut postings_reader).ok()? as usize;
        let max_postings_bytes = MAX_ENTRIES.checked_mul(POSTING_BYTES)?;
        if postings_len > max_postings_bytes {
            return None;
        }
        if postings_len > remaining_bytes(&mut postings_reader, postings_body_len)? {
            return None;
        }
        let mut postings_blob = vec![0u8; postings_len];
        postings_reader.read_exact(&mut postings_blob).ok()?;

        let mut lookup_magic = [0u8; 8];
        lookup_reader.read_exact(&mut lookup_magic).ok()?;
        if &lookup_magic != LOOKUP_MAGIC {
            return None;
        }
        if read_u32(&mut lookup_reader).ok()? != INDEX_VERSION {
            return None;
        }
        let entry_count = read_u32(&mut lookup_reader).ok()? as usize;
        if entry_count > MAX_ENTRIES {
            return None;
        }
        let remaining_lookup = remaining_bytes(&mut lookup_reader, lookup_body_len)?;
        let minimum_lookup_bytes = entry_count.checked_mul(LOOKUP_ENTRY_BYTES)?;
        if minimum_lookup_bytes > remaining_lookup {
            return None;
        }

        let mut postings = HashMap::new();
        let mut file_trigrams: HashMap<u32, Vec<u32>> = HashMap::new();

        for _ in 0..entry_count {
            let trigram = read_u32(&mut lookup_reader).ok()?;
            let offset = read_u64(&mut lookup_reader).ok()? as usize;
            let count = read_u32(&mut lookup_reader).ok()? as usize;
            if count > MAX_ENTRIES {
                return None;
            }
            let bytes_len = count.checked_mul(POSTING_BYTES)?;
            let end = offset.checked_add(bytes_len)?;
            if end > postings_blob.len() {
                return None;
            }

            let mut trigram_postings = Vec::with_capacity(count);
            for chunk in postings_blob[offset..end].chunks_exact(6) {
                let file_id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let posting = Posting {
                    file_id,
                    next_mask: chunk[4],
                    loc_mask: chunk[5],
                };
                trigram_postings.push(posting.clone());
                file_trigrams.entry(file_id).or_default().push(trigram);
            }
            postings.insert(trigram, trigram_postings);
        }

        Some(SearchIndex {
            postings,
            files,
            path_to_id,
            ready: false,
            project_root,
            git_head,
            max_file_size,
            file_trigrams,
            unindexed_files,
        })
    }

    pub(crate) fn stored_git_head(&self) -> Option<&str> {
        self.git_head.as_deref()
    }

    pub(crate) fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    pub(crate) fn verify_against_disk(&mut self, current_head: Option<String>) {
        self.git_head = current_head;
        verify_file_mtimes(self);
        self.ready = true;
    }

    pub(crate) fn rebuild_or_refresh(
        root: &Path,
        max_file_size: u64,
        current_head: Option<String>,
        baseline: Option<SearchIndex>,
    ) -> Self {
        if let Some(mut baseline) = baseline {
            baseline.project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            baseline.max_file_size = max_file_size;

            if baseline.git_head == current_head || current_head.is_none() {
                // HEAD matches, but files may have changed on disk since the index was
                // last written (e.g., uncommitted edits, stash pop, manual file changes
                // while OpenCode was closed). Verify mtimes and re-index stale files.
                // Non-git projects also use this per-file (path, mtime, size)
                // fingerprint so unchanged trees reuse the disk cache instead of
                // rebuilding every configure.
                baseline.git_head = current_head;
                verify_file_mtimes(&mut baseline);
                baseline.ready = true;
                return baseline;
            }

            if let (Some(previous), Some(current)) =
                (baseline.git_head.clone(), current_head.clone())
            {
                let project_root = baseline.project_root.clone();
                if apply_git_diff_updates(&mut baseline, &project_root, &previous, &current) {
                    baseline.git_head = Some(current);
                    verify_file_mtimes(&mut baseline);
                    baseline.ready = true;
                    return baseline;
                }
            }
        }

        SearchIndex::build_with_limit(root, max_file_size)
    }

    fn allocate_file_id(&mut self, path: &Path, size_hint: u64) -> Option<u32> {
        let file_id = u32::try_from(self.files.len()).ok()?;
        let metadata = fs::metadata(path).ok();
        let size = metadata
            .as_ref()
            .map_or(size_hint, |metadata| metadata.len());
        let modified = metadata
            .and_then(|metadata| metadata.modified().ok())
            .unwrap_or(UNIX_EPOCH);

        self.files.push(FileEntry {
            path: path.to_path_buf(),
            size,
            modified,
            content_hash: cache_freshness::zero_hash(),
        });
        self.path_to_id.insert(path.to_path_buf(), file_id);
        Some(file_id)
    }

    fn track_unindexed_file(&mut self, path: &Path, metadata: &fs::Metadata) {
        let Some(file_id) = self.allocate_file_id(path, metadata.len()) else {
            return;
        };
        self.unindexed_files.insert(file_id);
        self.file_trigrams.insert(file_id, Vec::new());
    }

    fn active_file_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.path_to_id.values().copied().collect();
        ids.sort_unstable();
        ids
    }

    fn is_active_file(&self, file_id: u32) -> bool {
        self.files
            .get(file_id as usize)
            .map(|file| !file.path.as_os_str().is_empty())
            .unwrap_or(false)
    }

    fn postings_for_trigram(&self, trigram: u32, filter: Option<PostingFilter>) -> Vec<u32> {
        let Some(postings) = self.postings.get(&trigram) else {
            return Vec::new();
        };

        let mut matches = Vec::with_capacity(postings.len());

        for posting in postings {
            if let Some(filter) = filter {
                // next_mask: bloom filter check — the character following this trigram in the
                // query must also appear after this trigram somewhere in the file.
                if filter.next_mask != 0 && posting.next_mask & filter.next_mask == 0 {
                    continue;
                }
                // NOTE: loc_mask (position mod 8) is stored for future adjacency checks
                // between consecutive trigram pairs, but is NOT used as a single-trigram
                // filter because the position in the query string has no relationship to
                // the position in the file. Using it here causes false negatives.
            }
            if self.is_active_file(posting.file_id) {
                matches.push(posting.file_id);
            }
        }

        matches
    }
}

fn search_candidate_file(
    file: &FileEntry,
    matcher: &SearchMatcher,
    max_results: usize,
    stop_after: usize,
    total_matches: &AtomicUsize,
    files_searched: &AtomicUsize,
    files_with_matches: &AtomicUsize,
    truncated: &AtomicBool,
) -> Vec<SharedGrepMatch> {
    if should_stop_search(truncated, total_matches, stop_after) {
        return Vec::new();
    }

    let content = match read_indexed_file_bytes(&file.path) {
        Some(content) => content,
        None => return Vec::new(),
    };
    // Defense in depth: even though indexing tries to filter binaries via
    // `is_binary_path` + full-content `is_binary_bytes`, we double-check at
    // query time. content_inspector is fast (~bytes-per-cycle on a small
    // preview) and this guarantees we never surface matches inside binary
    // files even if the indexer somehow let one through (e.g. file changed
    // between indexing and query).
    if is_binary_bytes(&content) {
        return Vec::new();
    }
    files_searched.fetch_add(1, Ordering::Relaxed);

    let shared_path = Arc::new(file.path.clone());
    let mut matches = Vec::new();
    let mut line_starts = None;
    let mut seen_lines = HashSet::new();
    let mut matched_this_file = false;

    match matcher {
        SearchMatcher::Literal(LiteralSearch::CaseSensitive(needle)) => {
            let finder = memchr::memmem::Finder::new(needle);
            let mut start = 0;

            while let Some(position) = finder.find(&content[start..]) {
                if should_stop_search(truncated, total_matches, stop_after) {
                    break;
                }

                let offset = start + position;
                start = offset + 1;

                let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(&content));
                let (line, column, line_text) = line_details_bytes(&content, line_starts, offset);
                if !seen_lines.insert(line) {
                    continue;
                }

                matched_this_file = true;
                let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
                if match_number > max_results {
                    truncated.store(true, Ordering::Relaxed);
                    break;
                }

                let end = offset + needle.len();
                matches.push(SharedGrepMatch {
                    file: shared_path.clone(),
                    line,
                    column,
                    line_text,
                    match_text: String::from_utf8_lossy(&content[offset..end]).into_owned(),
                });
            }
        }
        SearchMatcher::Literal(LiteralSearch::AsciiCaseInsensitive(needle)) => {
            let search_content = content.to_ascii_lowercase();
            let finder = memchr::memmem::Finder::new(needle);
            let mut start = 0;

            while let Some(position) = finder.find(&search_content[start..]) {
                if should_stop_search(truncated, total_matches, stop_after) {
                    break;
                }

                let offset = start + position;
                start = offset + 1;

                let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(&content));
                let (line, column, line_text) = line_details_bytes(&content, line_starts, offset);
                if !seen_lines.insert(line) {
                    continue;
                }

                matched_this_file = true;
                let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
                if match_number > max_results {
                    truncated.store(true, Ordering::Relaxed);
                    break;
                }

                let end = offset + needle.len();
                matches.push(SharedGrepMatch {
                    file: shared_path.clone(),
                    line,
                    column,
                    line_text,
                    match_text: String::from_utf8_lossy(&content[offset..end]).into_owned(),
                });
            }
        }
        SearchMatcher::Regex(regex) => {
            for matched in regex.find_iter(&content) {
                if should_stop_search(truncated, total_matches, stop_after) {
                    break;
                }

                let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(&content));
                let (line, column, line_text) =
                    line_details_bytes(&content, line_starts, matched.start());
                if !seen_lines.insert(line) {
                    continue;
                }

                matched_this_file = true;
                let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
                if match_number > max_results {
                    truncated.store(true, Ordering::Relaxed);
                    break;
                }

                matches.push(SharedGrepMatch {
                    file: shared_path.clone(),
                    line,
                    column,
                    line_text,
                    match_text: String::from_utf8_lossy(matched.as_bytes()).into_owned(),
                });
            }
        }
    }

    if matched_this_file {
        files_with_matches.fetch_add(1, Ordering::Relaxed);
    }

    matches
}

fn should_stop_search(
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
) -> bool {
    truncated.load(Ordering::Relaxed) && total_matches.load(Ordering::Relaxed) >= stop_after
}

fn intersect_sorted_ids(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut merged = Vec::with_capacity(left.len().min(right.len()));
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                merged.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    merged
}

fn union_sorted_ids(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => {
                merged.push(left[left_index]);
                left_index += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.push(right[right_index]);
                right_index += 1;
            }
            std::cmp::Ordering::Equal => {
                merged.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    merged.extend_from_slice(&left[left_index..]);
    merged.extend_from_slice(&right[right_index..]);
    merged
}

pub fn decompose_regex(pattern: &str) -> RegexQuery {
    let hir = match regex_syntax::parse(pattern) {
        Ok(hir) => hir,
        Err(_) => return RegexQuery::default(),
    };

    let build = build_query(&hir);
    build.into_query()
}

pub fn pack_trigram(a: u8, b: u8, c: u8) -> u32 {
    ((a as u32) << 16) | ((b as u32) << 8) | c as u32
}

pub fn normalize_char(c: u8) -> u8 {
    c.to_ascii_lowercase()
}

pub fn extract_trigrams(content: &[u8]) -> Vec<(u32, u8, usize)> {
    if content.len() < 3 {
        return Vec::new();
    }

    let mut trigrams = Vec::with_capacity(content.len().saturating_sub(2));
    for start in 0..=content.len() - 3 {
        let trigram = pack_trigram(
            normalize_char(content[start]),
            normalize_char(content[start + 1]),
            normalize_char(content[start + 2]),
        );
        let next_char = content.get(start + 3).copied().unwrap_or(EOF_SENTINEL);
        trigrams.push((trigram, next_char, start));
    }
    trigrams
}

pub fn query_trigrams_from_tokens(tokens: &[&str]) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for token in tokens {
        for (trigram, _, _) in extract_trigrams(token.as_bytes()) {
            if seen.insert(trigram) {
                out.push(trigram);
            }
        }
    }
    out
}

pub fn lexical_score(index: &SearchIndex, query_trigrams: &[u32], file_id: u32) -> f32 {
    if query_trigrams.is_empty() {
        return 0.0;
    }

    let mut hits = 0u32;
    for &trigram in query_trigrams {
        if let Some(postings) = index.postings.get(&trigram) {
            if postings
                .binary_search_by(|posting| posting.file_id.cmp(&file_id))
                .is_ok()
            {
                hits += 1;
            }
        }
    }

    if hits == 0 {
        return 0.0;
    }

    let file_trigram_count = index
        .file_trigrams
        .get(&file_id)
        .map_or(1, |trigrams| trigrams.len().max(1)) as f32;
    (hits as f32) / (1.0 + file_trigram_count.ln())
}

pub fn resolve_cache_dir(project_root: &Path, storage_dir: Option<&Path>) -> PathBuf {
    // Respect AFT_CACHE_DIR for testing — prevents tests from polluting the user's storage
    if let Some(override_dir) = std::env::var_os("AFT_CACHE_DIR") {
        return PathBuf::from(override_dir)
            .join("index")
            .join(project_cache_key(project_root));
    }
    // Use configured storage dir (from plugin, XDG-compliant)
    if let Some(dir) = storage_dir {
        return dir.join("index").join(project_cache_key(project_root));
    }
    // Fallback to ~/.cache/aft/ (legacy, for standalone binary usage).
    // On Windows `HOME` is typically unset, so try `USERPROFILE` next.
    // If neither is set, fall back to a temp directory rather than `"."`
    // because the search-index code reads/writes absolute paths.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache")
        .join("aft")
        .join("index")
        .join(project_cache_key(project_root))
}

pub(crate) fn build_path_filters(
    include: &[String],
    exclude: &[String],
) -> Result<PathFilters, String> {
    Ok(PathFilters {
        includes: build_globset(include)?,
        excludes: build_globset(exclude)?,
    })
}

pub(crate) fn walk_project_files(root: &Path, filters: &PathFilters) -> Vec<PathBuf> {
    walk_project_files_from(root, root, filters)
}

pub(crate) fn walk_project_files_from(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(search_root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                return !matches!(
                    name.as_ref(),
                    "node_modules"
                        | "target"
                        | "venv"
                        | ".venv"
                        | ".git"
                        | "__pycache__"
                        | ".tox"
                        | "dist"
                        | "build"
                );
            }
            true
        });

    let mut files = Vec::new();
    for entry in builder.build().filter_map(|entry| entry.ok()) {
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        let path = entry.into_path();
        if filters.matches(filter_root, &path) {
            files.push(path);
        }
    }

    sort_paths_by_mtime_desc(&mut files);
    files
}

pub(crate) fn read_searchable_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if is_binary_bytes(&bytes) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn read_indexed_file_bytes(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

pub(crate) fn relative_to_root(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn cache_relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let normalized_root = normalize_path(root);
    let normalized_path = normalize_path(path);
    let relative = normalized_path.strip_prefix(&normalized_root).ok()?;
    validate_cached_relative_path(relative)
}

pub(crate) fn cached_path_under_root(root: &Path, relative_path: &Path) -> Option<PathBuf> {
    let relative = validate_cached_relative_path(relative_path)?;
    let normalized_root = normalize_path(root);
    let full_path = normalize_path(&normalized_root.join(relative));
    full_path.starts_with(&normalized_root).then_some(full_path)
}

pub(crate) fn validate_cached_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

/// Sort paths newest-first by mtime, falling back to lexicographic order.
///
/// Pre-v0.15.2 this called `path_modified_time(...)` directly inside the
/// `sort_by()` closure. That made the comparator non-deterministic — a
/// `stat()` syscall for the same path can return different values across
/// invocations (file edited mid-sort, file deleted, OS clock adjustments,
/// concurrent file-watcher activity), and Rust's slice::sort panics at
/// runtime when it detects a non-total-order comparator. CI hit this on
/// a Pi e2e test where the bridge invalidated files in parallel with grep.
///
/// Fix: snapshot mtimes ONCE into a HashMap before sorting, then look up
/// from the map inside the closure. Pure function ⇒ guaranteed total order.
pub(crate) fn sort_paths_by_mtime_desc(paths: &mut [PathBuf]) {
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::with_capacity(paths.len());
    for path in paths.iter() {
        mtimes
            .entry(path.clone())
            .or_insert_with(|| path_modified_time(path));
    }
    paths.sort_by(|left, right| {
        let left_mtime = mtimes.get(left).and_then(|v| *v);
        let right_mtime = mtimes.get(right).and_then(|v| *v);
        right_mtime.cmp(&left_mtime).then_with(|| left.cmp(right))
    });
}

/// See `sort_paths_by_mtime_desc` for why mtimes are snapshotted ahead of
/// the sort. Same fix, applied to grep matches that share files.
pub(crate) fn sort_grep_matches_by_mtime_desc(matches: &mut [GrepMatch], project_root: &Path) {
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::new();
    for m in matches.iter() {
        mtimes.entry(m.file.clone()).or_insert_with(|| {
            let resolved = resolve_match_path(project_root, &m.file);
            path_modified_time(&resolved)
        });
    }
    matches.sort_by(|left, right| {
        let left_mtime = mtimes.get(&left.file).and_then(|v| *v);
        let right_mtime = mtimes.get(&right.file).and_then(|v| *v);
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
}

/// See `sort_paths_by_mtime_desc` for why mtimes are snapshotted ahead of
/// the sort. The cached lookup function `modified_for_path` is fast (in-memory
/// table from the search index), but it can still return different values if
/// the file is modified mid-sort. Snapshot once.
fn sort_shared_grep_matches_by_cached_mtime_desc<F>(
    matches: &mut [SharedGrepMatch],
    modified_for_path: F,
) where
    F: Fn(&Path) -> Option<SystemTime>,
{
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::with_capacity(matches.len());
    for m in matches.iter() {
        let path = m.file.as_path().to_path_buf();
        mtimes
            .entry(path.clone())
            .or_insert_with(|| modified_for_path(&path));
    }
    matches.sort_by(|left, right| {
        let left_mtime = mtimes.get(left.file.as_path()).and_then(|v| *v);
        let right_mtime = mtimes.get(right.file.as_path()).and_then(|v| *v);
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left.file.as_path().cmp(right.file.as_path()))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
}

pub(crate) fn resolve_search_scope(project_root: &Path, path: Option<&str>) -> SearchScope {
    let resolved_project_root = canonicalize_or_normalize(project_root);
    let root = match path {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                canonicalize_or_normalize(&path)
            } else {
                normalize_path(&resolved_project_root.join(path))
            }
        }
        None => resolved_project_root.clone(),
    };

    let use_index = is_within_search_root(&resolved_project_root, &root);
    SearchScope { root, use_index }
}

pub(crate) fn is_binary_bytes(content: &[u8]) -> bool {
    content_inspector::inspect(content).is_binary()
}

pub(crate) fn current_git_head(root: &Path) -> Option<String> {
    run_git(root, &["rev-parse", "HEAD"])
}

pub fn project_cache_key(project_root: &Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();

    if let Some(root_commit) = run_git(project_root, &["rev-list", "--max-parents=0", "HEAD"]) {
        // Git repo: root commit is the unique identity.
        // Same repo cloned anywhere produces the same key.
        hasher.update(root_commit.as_bytes());
    } else {
        // Non-git project: use the canonical filesystem path as identity.
        let canonical_root = canonicalize_or_normalize(project_root);
        hasher.update(canonical_root.to_string_lossy().as_bytes());
    }

    let digest = format!("{:x}", hasher.finalize());
    digest[..16].to_string()
}

impl PathFilters {
    fn matches(&self, root: &Path, path: &Path) -> bool {
        let relative = to_glob_path(&relative_to_root(root, path));
        if self
            .includes
            .as_ref()
            .is_some_and(|includes| !includes.is_match(&relative))
        {
            return false;
        }
        if self
            .excludes
            .as_ref()
            .is_some_and(|excludes| excludes.is_match(&relative))
        {
            return false;
        }
        true
    }
}

fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn resolve_match_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn path_modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !result.pop() {
                    result.push(component);
                }
            }
            Component::CurDir => {}
            _ => result.push(component),
        }
    }
    result
}

/// Verify stored file mtimes against disk. Re-index any files whose mtime changed
/// since the index was last written. Also detect new files and deleted files.
fn verify_file_mtimes(index: &mut SearchIndex) {
    // Collect stale files (mtime mismatch or deleted)
    let mut stale_paths = Vec::new();
    for entry in &index.files {
        if entry.path.as_os_str().is_empty() {
            continue; // tombstoned entry
        }
        let cached = FileFreshness {
            mtime: entry.modified,
            size: entry.size,
            content_hash: entry.content_hash,
        };
        match cache_freshness::verify_file(&entry.path, &cached) {
            FreshnessVerdict::HotFresh | FreshnessVerdict::ContentFresh { .. } => {}
            FreshnessVerdict::Stale | FreshnessVerdict::Deleted => {
                stale_paths.push(entry.path.clone())
            }
        }
    }

    for entry in &mut index.files {
        if entry.path.as_os_str().is_empty() {
            continue;
        }
        let cached = FileFreshness {
            mtime: entry.modified,
            size: entry.size,
            content_hash: entry.content_hash,
        };
        if let FreshnessVerdict::ContentFresh {
            new_mtime,
            new_size,
        } = cache_freshness::verify_file(&entry.path, &cached)
        {
            entry.modified = new_mtime;
            entry.size = new_size;
        }
    }

    // Re-index stale files
    for path in &stale_paths {
        index.update_file(path);
    }

    // Detect new files not in the index
    let filters = PathFilters::default();
    for path in walk_project_files(&index.project_root, &filters) {
        if !index.path_to_id.contains_key(&path) {
            index.update_file(&path);
        }
    }

    if !stale_paths.is_empty() {
        crate::slog_info!(
            "search index: refreshed {} stale file(s) from disk cache",
            stale_paths.len()
        );
    }
}

fn is_within_search_root(search_root: &Path, path: &Path) -> bool {
    normalize_path(path).starts_with(normalize_path(search_root))
}

impl QueryBuild {
    fn into_query(self) -> RegexQuery {
        let mut query = RegexQuery::default();

        for run in self.and_runs {
            add_run_to_and_query(&mut query, &run);
        }

        for group in self.or_groups {
            let mut trigrams = BTreeSet::new();
            let mut filters = HashMap::new();
            for run in group {
                for (trigram, filter) in trigram_filters(&run) {
                    trigrams.insert(trigram);
                    merge_filter(filters.entry(trigram).or_default(), filter);
                }
            }
            if !trigrams.is_empty() {
                query.or_groups.push(trigrams.into_iter().collect());
                query.or_filters.push(filters);
            }
        }

        query
    }
}

fn build_query(hir: &Hir) -> QueryBuild {
    match hir.kind() {
        HirKind::Literal(literal) => {
            if literal.0.len() >= 3 {
                QueryBuild {
                    and_runs: vec![literal.0.to_vec()],
                    or_groups: Vec::new(),
                }
            } else {
                QueryBuild::default()
            }
        }
        HirKind::Capture(capture) => build_query(&capture.sub),
        HirKind::Concat(parts) => {
            let mut build = QueryBuild::default();
            for part in parts {
                let part_build = build_query(part);
                build.and_runs.extend(part_build.and_runs);
                build.or_groups.extend(part_build.or_groups);
            }
            build
        }
        HirKind::Alternation(parts) => {
            let mut group = Vec::new();
            for part in parts {
                let Some(mut choices) = guaranteed_run_choices(part) else {
                    return QueryBuild::default();
                };
                group.append(&mut choices);
            }
            if group.is_empty() {
                QueryBuild::default()
            } else {
                QueryBuild {
                    and_runs: Vec::new(),
                    or_groups: vec![group],
                }
            }
        }
        HirKind::Repetition(repetition) => {
            if repetition.min == 0 {
                QueryBuild::default()
            } else {
                build_query(&repetition.sub)
            }
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => QueryBuild::default(),
    }
}

fn guaranteed_run_choices(hir: &Hir) -> Option<Vec<Vec<u8>>> {
    match hir.kind() {
        HirKind::Literal(literal) => {
            if literal.0.len() >= 3 {
                Some(vec![literal.0.to_vec()])
            } else {
                None
            }
        }
        HirKind::Capture(capture) => guaranteed_run_choices(&capture.sub),
        HirKind::Concat(parts) => {
            let mut runs = Vec::new();
            for part in parts {
                if let Some(mut part_runs) = guaranteed_run_choices(part) {
                    runs.append(&mut part_runs);
                }
            }
            if runs.is_empty() {
                None
            } else {
                Some(runs)
            }
        }
        HirKind::Alternation(parts) => {
            let mut runs = Vec::new();
            for part in parts {
                let Some(mut part_runs) = guaranteed_run_choices(part) else {
                    return None;
                };
                runs.append(&mut part_runs);
            }
            if runs.is_empty() {
                None
            } else {
                Some(runs)
            }
        }
        HirKind::Repetition(repetition) => {
            if repetition.min == 0 {
                None
            } else {
                guaranteed_run_choices(&repetition.sub)
            }
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => None,
    }
}

fn add_run_to_and_query(query: &mut RegexQuery, run: &[u8]) {
    for (trigram, filter) in trigram_filters(run) {
        if !query.and_trigrams.contains(&trigram) {
            query.and_trigrams.push(trigram);
        }
        merge_filter(query.and_filters.entry(trigram).or_default(), filter);
    }
}

fn trigram_filters(run: &[u8]) -> Vec<(u32, PostingFilter)> {
    let mut filters: BTreeMap<u32, PostingFilter> = BTreeMap::new();
    for (trigram, next_char, position) in extract_trigrams(run) {
        let entry: &mut PostingFilter = filters.entry(trigram).or_default();
        if next_char != EOF_SENTINEL {
            entry.next_mask |= mask_for_next_char(next_char);
        }
        entry.loc_mask |= mask_for_position(position);
    }
    filters.into_iter().collect()
}

fn merge_filter(target: &mut PostingFilter, filter: PostingFilter) {
    target.next_mask |= filter.next_mask;
    target.loc_mask |= filter.loc_mask;
}

fn mask_for_next_char(next_char: u8) -> u8 {
    let bit = (normalize_char(next_char).wrapping_mul(31) & 7) as u32;
    1u8 << bit
}

fn mask_for_position(position: usize) -> u8 {
    1u8 << (position % 8)
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| error.to_string())?;
        builder.add(glob);
    }
    builder.build().map(Some).map_err(|error| error.to_string())
}

fn read_u32<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut buffer = [0u8; 4];
    reader.read_exact(&mut buffer)?;
    Ok(u32::from_le_bytes(buffer))
}

fn read_u64<R: Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut buffer = [0u8; 8];
    reader.read_exact(&mut buffer)?;
    Ok(u64::from_le_bytes(buffer))
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn verify_crc32_bytes_slice(bytes: &[u8]) -> std::io::Result<()> {
    let Some((body, stored)) = bytes.split_last_chunk::<4>() else {
        return Err(std::io::Error::other("search index checksum missing"));
    };
    let expected = u32::from_le_bytes(*stored);
    let actual = crc32fast::hash(body);
    if actual != expected {
        return Err(std::io::Error::other("search index checksum mismatch"));
    }
    Ok(())
}

fn remaining_bytes<R: Seek>(reader: &mut R, total_len: usize) -> Option<usize> {
    let pos = usize::try_from(reader.stream_position().ok()?).ok()?;
    total_len.checked_sub(pos)
}

fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn apply_git_diff_updates(index: &mut SearchIndex, root: &Path, from: &str, to: &str) -> bool {
    let diff_range = format!("{}..{}", from, to);
    let output = match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-status", "-M", &diff_range])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };

    if !output.status.success() {
        return false;
    }

    let Ok(diff) = String::from_utf8(output.stdout) else {
        return false;
    };

    for line in diff.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let mut fields = line.split('\t');
        let Some(status) = fields.next() else {
            continue;
        };

        if status.starts_with('R') {
            let Some(old_path) = fields
                .next()
                .and_then(|path| cached_path_under_root(root, &PathBuf::from(path)))
            else {
                continue;
            };
            let Some(new_path) = fields
                .next()
                .and_then(|path| cached_path_under_root(root, &PathBuf::from(path)))
            else {
                continue;
            };
            index.remove_file(&old_path);
            index.update_file(&new_path);
            continue;
        }

        let Some(path) = fields
            .next()
            .and_then(|path| cached_path_under_root(root, &PathBuf::from(path)))
        else {
            continue;
        };
        if status.starts_with('D') || !path.exists() {
            index.remove_file(&path);
        } else {
            index.update_file(&path);
        }
    }

    true
}

fn is_binary_path(path: &Path, size: u64) -> bool {
    if size == 0 {
        return false;
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return true,
    };

    let mut preview = vec![0u8; PREVIEW_BYTES.min(size as usize)];
    match file.read(&mut preview) {
        Ok(read) => is_binary_bytes(&preview[..read]),
        Err(_) => true,
    }
}

fn line_starts_bytes(content: &[u8]) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (index, byte) in content.iter().copied().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_details_bytes(content: &[u8], line_starts: &[usize], offset: usize) -> (u32, u32, String) {
    let line_index = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_starts.get(line_index).copied().unwrap_or(0);
    let line_end = content[line_start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|length| line_start + length)
        .unwrap_or(content.len());
    let mut line_slice = &content[line_start..line_end];
    if line_slice.ends_with(b"\r") {
        line_slice = &line_slice[..line_slice.len() - 1];
    }
    let line_text = String::from_utf8_lossy(line_slice).into_owned();
    let column = String::from_utf8_lossy(&content[line_start..offset])
        .chars()
        .count() as u32
        + 1;
    (line_index as u32 + 1, column, line_text)
}

fn to_glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn extract_trigrams_tracks_next_char_and_position() {
        let trigrams = extract_trigrams(b"Rust");
        assert_eq!(trigrams.len(), 2);
        assert_eq!(trigrams[0], (pack_trigram(b'r', b'u', b's'), b't', 0));
        assert_eq!(
            trigrams[1],
            (pack_trigram(b'u', b's', b't'), EOF_SENTINEL, 1)
        );
    }

    #[test]
    fn decompose_regex_extracts_literals_and_alternations() {
        let query = decompose_regex("abc(def|ghi)xyz");
        assert!(query.and_trigrams.contains(&pack_trigram(b'a', b'b', b'c')));
        assert!(query.and_trigrams.contains(&pack_trigram(b'x', b'y', b'z')));
        assert_eq!(query.or_groups.len(), 1);
        assert!(query.or_groups[0].contains(&pack_trigram(b'd', b'e', b'f')));
        assert!(query.or_groups[0].contains(&pack_trigram(b'g', b'h', b'i')));
    }

    #[test]
    fn candidates_intersect_posting_lists() {
        let mut index = SearchIndex::new();
        let dir = tempfile::tempdir().expect("create temp dir");
        let alpha = dir.path().join("alpha.txt");
        let beta = dir.path().join("beta.txt");
        fs::write(&alpha, "abcdef").expect("write alpha");
        fs::write(&beta, "abcxyz").expect("write beta");
        index.project_root = dir.path().to_path_buf();
        index.index_file(&alpha, b"abcdef");
        index.index_file(&beta, b"abcxyz");

        let query = RegexQuery {
            and_trigrams: vec![
                pack_trigram(b'a', b'b', b'c'),
                pack_trigram(b'd', b'e', b'f'),
            ],
            ..RegexQuery::default()
        };

        let candidates = index.candidates(&query);
        assert_eq!(candidates.len(), 1);
        assert_eq!(index.files[candidates[0] as usize].path, alpha);
    }

    #[test]
    fn candidates_apply_bloom_filters() {
        let mut index = SearchIndex::new();
        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("sample.txt");
        fs::write(&file, "abcd efgh").expect("write sample");
        index.project_root = dir.path().to_path_buf();
        index.index_file(&file, b"abcd efgh");

        let trigram = pack_trigram(b'a', b'b', b'c');
        let matching_filter = PostingFilter {
            next_mask: mask_for_next_char(b'd'),
            loc_mask: mask_for_position(0),
        };
        let non_matching_filter = PostingFilter {
            next_mask: mask_for_next_char(b'z'),
            loc_mask: mask_for_position(0),
        };

        assert_eq!(
            index
                .postings_for_trigram(trigram, Some(matching_filter))
                .len(),
            1
        );
        assert!(index
            .postings_for_trigram(trigram, Some(non_matching_filter))
            .is_empty());
    }

    #[test]
    fn disk_round_trip_preserves_postings_and_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("src.txt");
        fs::write(&file, "abcdef").expect("write source");

        let mut index = SearchIndex::build(&project);
        index.git_head = Some("deadbeef".to_string());
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, index.git_head.as_deref());

        let loaded =
            SearchIndex::read_from_disk(&cache_dir, &project).expect("load index from disk");
        assert_eq!(loaded.stored_git_head(), Some("deadbeef"));
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            relative_to_root(&loaded.project_root, &loaded.files[0].path),
            PathBuf::from("src.txt")
        );
        assert_eq!(loaded.postings.len(), index.postings.len());
        assert!(loaded
            .postings
            .contains_key(&pack_trigram(b'a', b'b', b'c')));
    }

    #[test]
    fn cache_path_helpers_reject_absolute_and_parent_paths() {
        let root = PathBuf::from("/tmp/aft-project");

        assert_eq!(
            cache_relative_path(&root, &root.join("src/lib.rs")),
            Some(PathBuf::from("src/lib.rs"))
        );
        assert!(cache_relative_path(&root, Path::new("/tmp/outside.rs")).is_none());
        assert!(cached_path_under_root(&root, Path::new("../outside.rs")).is_none());
        assert!(cached_path_under_root(&root, Path::new("/tmp/outside.rs")).is_none());
        assert_eq!(
            cached_path_under_root(&root, Path::new("src/./lib.rs")),
            Some(root.join("src/lib.rs"))
        );
    }

    #[test]
    fn refresh_after_head_change_removes_renames_and_detects_local_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let canonical_project = fs::canonicalize(&project).expect("canonical project");
        fs::write(project.join("old.txt"), "old token\n").expect("write old");
        fs::write(project.join("unchanged.txt"), "before\n").expect("write unchanged");

        Command::new("git")
            .arg("init")
            .arg(&project)
            .status()
            .expect("git init");
        for args in [
            ["config", "user.email", "aft@example.invalid"],
            ["config", "user.name", "AFT Test"],
        ] {
            Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .status()
                .expect("git config");
        }
        Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["add", "."])
            .status()
            .expect("git add initial");
        Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["commit", "-m", "initial"])
            .status()
            .expect("git commit initial");
        let previous = run_git(&project, &["rev-parse", "HEAD"]).expect("previous head");
        let mut baseline = SearchIndex::build(&project);
        baseline.git_head = Some(previous.clone());

        fs::rename(project.join("old.txt"), project.join("new.txt")).expect("rename file");
        Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["add", "-A"])
            .status()
            .expect("git add rename");
        Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["commit", "-m", "rename"])
            .status()
            .expect("git commit rename");
        let current = run_git(&project, &["rev-parse", "HEAD"]).expect("current head");

        fs::write(project.join("unchanged.txt"), "after local edit\n").expect("local edit");
        fs::write(project.join("untracked.txt"), "untracked token\n").expect("untracked");

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            Some(current),
            Some(baseline),
        );

        assert!(!refreshed
            .path_to_id
            .contains_key(&canonical_project.join("old.txt")));
        assert!(refreshed
            .path_to_id
            .contains_key(&canonical_project.join("new.txt")));
        assert!(refreshed
            .path_to_id
            .contains_key(&canonical_project.join("untracked.txt")));
        let matches =
            refreshed.search_grep("after local edit", true, &[], &[], &canonical_project, 10);
        assert_eq!(matches.matches.len(), 1);
    }

    #[test]
    fn read_from_disk_rejects_corrupt_postings_checksum() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");

        let index = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, None);

        let cache_path = cache_dir.join("cache.bin");
        let mut bytes = fs::read(&cache_path).expect("read cache");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        fs::write(&cache_path, bytes).expect("write corrupted cache");

        assert!(SearchIndex::read_from_disk(&cache_dir, &project).is_none());
    }

    #[test]
    fn write_to_disk_uses_temp_files_and_cleans_them_up() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");

        let index = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, None);

        assert!(cache_dir.join("cache.bin").is_file());
        assert!(fs::read_dir(&cache_dir)
            .expect("read cache dir")
            .all(|entry| !entry
                .expect("cache entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")));
    }

    #[test]
    fn concurrent_search_index_writes_do_not_corrupt() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef\n").expect("write source");
        let cache_dir = dir.path().join("cache");

        let a_project = project.clone();
        let a_cache = cache_dir.clone();
        let a = std::thread::spawn(move || {
            let _lock = CacheLock::acquire(&a_cache).expect("acquire cache lock a");
            let index = SearchIndex::build(&a_project);
            index.write_to_disk(&a_cache, None);
        });
        let b_project = project.clone();
        let b_cache = cache_dir.clone();
        let b = std::thread::spawn(move || {
            let _lock = CacheLock::acquire(&b_cache).expect("acquire cache lock b");
            let index = SearchIndex::build(&b_project);
            index.write_to_disk(&b_cache, None);
        });
        a.join().expect("writer a");
        b.join().expect("writer b");

        assert!(SearchIndex::read_from_disk(&cache_dir, &project).is_some());
    }

    #[test]
    fn search_index_atomic_rename_survives_partial_write() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).expect("create cache dir");
        fs::write(cache_dir.join("cache.bin.tmp.1.1"), b"partial").expect("write partial tmp");

        assert!(SearchIndex::read_from_disk(&cache_dir, dir.path()).is_none());
    }

    #[test]
    fn project_cache_key_includes_checkout_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let source = dir.path().join("source");
        fs::create_dir_all(&source).expect("create source repo dir");
        fs::write(source.join("tracked.txt"), "content\n").expect("write tracked file");

        assert!(Command::new("git")
            .current_dir(&source)
            .args(["init"])
            .status()
            .expect("init git repo")
            .success());
        assert!(Command::new("git")
            .current_dir(&source)
            .args(["add", "."])
            .status()
            .expect("git add")
            .success());
        assert!(Command::new("git")
            .current_dir(&source)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "-m",
                "initial",
            ])
            .status()
            .expect("git commit")
            .success());

        let clone = dir.path().join("clone");
        assert!(Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&source)
            .arg(&clone)
            .status()
            .expect("git clone")
            .success());

        let source_key = project_cache_key(&source);
        let clone_key = project_cache_key(&clone);

        assert_eq!(source_key.len(), 16);
        assert_eq!(clone_key.len(), 16);
        // Same repo (same root commit) → same cache key regardless of clone path
        assert_eq!(source_key, clone_key);
    }

    #[test]
    fn git_head_unchanged_picks_up_local_edits() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("repo");
        fs::create_dir_all(&project).expect("create repo dir");
        let file = project.join("tracked.txt");
        fs::write(&file, "oldtoken\n").expect("write file");
        assert!(Command::new("git")
            .current_dir(&project)
            .arg("init")
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(&project)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(&project)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "-m",
                "initial"
            ])
            .status()
            .unwrap()
            .success());
        let head = current_git_head(&project);
        let mut baseline = SearchIndex::build(&project);
        baseline.git_head = head.clone();
        fs::write(&file, "newtoken\n").expect("edit tracked file");

        let refreshed =
            SearchIndex::rebuild_or_refresh(&project, DEFAULT_MAX_FILE_SIZE, head, Some(baseline));
        let result = refreshed.search_grep("newtoken", true, &[], &[], &project, 10);

        assert_eq!(result.total_matches, 1);
    }

    #[test]
    fn non_git_project_reuses_cache_when_files_unchanged() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("file.txt"), "unchangedtoken\n").expect("write file");
        let baseline = SearchIndex::build(&project);
        let baseline_file_count = baseline.file_count();

        let refreshed =
            SearchIndex::rebuild_or_refresh(&project, DEFAULT_MAX_FILE_SIZE, None, Some(baseline));

        assert_eq!(refreshed.file_count(), baseline_file_count);
        assert_eq!(
            refreshed
                .search_grep("unchangedtoken", true, &[], &[], &project, 10)
                .total_matches,
            1
        );
    }

    #[test]
    fn resolve_search_scope_disables_index_for_external_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&outside).expect("create outside dir");

        let scope = resolve_search_scope(&project, outside.to_str());

        assert_eq!(
            scope.root,
            fs::canonicalize(&outside).expect("canonicalize outside")
        );
        assert!(!scope.use_index);
    }

    #[test]
    fn grep_filters_matches_to_search_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let docs = project.join("docs");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&docs).expect("create docs dir");
        fs::write(src.join("main.rs"), "pub struct SearchIndex;\n").expect("write src file");
        fs::write(docs.join("guide.md"), "SearchIndex guide\n").expect("write docs file");

        let index = SearchIndex::build(&project);
        let result = index.search_grep("SearchIndex", true, &[], &[], &src, 10);

        assert_eq!(result.files_searched, 1);
        assert_eq!(result.files_with_matches, 1);
        assert_eq!(result.matches.len(), 1);
        // Index stores canonicalized paths; on macOS /var → /private/var
        let expected = fs::canonicalize(src.join("main.rs")).expect("canonicalize");
        assert_eq!(result.matches[0].file, expected);
    }

    #[test]
    fn grep_deduplicates_multiple_matches_on_same_line() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("main.rs"), "SearchIndex SearchIndex\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let result = index.search_grep("SearchIndex", true, &[], &[], &src, 10);

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn grep_reports_total_matches_before_truncation() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("main.rs"), "SearchIndex\nSearchIndex\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let result = index.search_grep("SearchIndex", true, &[], &[], &src, 1);

        assert_eq!(result.total_matches, 2);
        assert_eq!(result.matches.len(), 1);
        assert!(result.truncated);
    }

    #[test]
    fn glob_filters_results_to_search_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let scripts = project.join("scripts");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&scripts).expect("create scripts dir");
        fs::write(src.join("main.rs"), "pub fn main() {}\n").expect("write src file");
        fs::write(scripts.join("tool.rs"), "pub fn tool() {}\n").expect("write scripts file");

        let index = SearchIndex::build(&project);
        let files = index.glob("**/*.rs", &src);

        assert_eq!(
            files,
            vec![fs::canonicalize(src.join("main.rs")).expect("canonicalize src file")]
        );
    }

    #[test]
    fn glob_includes_hidden_and_binary_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let hidden_dir = project.join(".hidden");
        fs::create_dir_all(&hidden_dir).expect("create hidden dir");
        let hidden_file = hidden_dir.join("data.bin");
        fs::write(&hidden_file, [0u8, 159, 146, 150]).expect("write binary file");

        let index = SearchIndex::build(&project);
        let files = index.glob("**/*.bin", &project);

        assert_eq!(
            files,
            vec![fs::canonicalize(hidden_file).expect("canonicalize binary file")]
        );
    }

    #[test]
    fn read_from_disk_rejects_invalid_nanos() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).expect("create cache dir");

        let mut postings = Vec::new();
        postings.extend_from_slice(INDEX_MAGIC);
        postings.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        postings.extend_from_slice(&0u32.to_le_bytes());
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(&DEFAULT_MAX_FILE_SIZE.to_le_bytes());
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(b"/");
        postings.push(0u8);
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(&0u64.to_le_bytes());
        postings.extend_from_slice(&0u64.to_le_bytes());
        postings.extend_from_slice(&1_000_000_000u32.to_le_bytes());
        postings.extend_from_slice(b"a");
        postings.extend_from_slice(&0u64.to_le_bytes());

        let mut lookup = Vec::new();
        lookup.extend_from_slice(LOOKUP_MAGIC);
        lookup.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        lookup.extend_from_slice(&0u32.to_le_bytes());

        let postings_checksum = crc32fast::hash(&postings);
        postings.extend_from_slice(&postings_checksum.to_le_bytes());
        let lookup_checksum = crc32fast::hash(&lookup);
        lookup.extend_from_slice(&lookup_checksum.to_le_bytes());
        let mut cache = Vec::new();
        cache.extend_from_slice(&CACHE_MAGIC.to_le_bytes());
        cache.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        cache.extend_from_slice(&(postings.len() as u64).to_le_bytes());
        cache.extend_from_slice(&postings);
        cache.extend_from_slice(&lookup);
        fs::write(cache_dir.join("cache.bin"), cache).expect("write cache");

        assert!(SearchIndex::read_from_disk(&cache_dir, dir.path()).is_none());
    }

    /// Regression: v0.15.2 — sort_paths_by_mtime_desc panicked when files
    /// changed between cmp() calls.
    ///
    /// Pre-fix, the sort closure called `path_modified_time(path)` directly,
    /// which does a `stat()` syscall. If the file was deleted, modified, or
    /// touched mid-sort, the comparator returned different values for the
    /// same input pair on different invocations. Rust's slice::sort detects
    /// this and panics with "user-provided comparison function does not
    /// correctly implement a total order".
    ///
    /// CI hit this on a Pi e2e test (workflow run 24887807972) where the
    /// bridge invalidated files in parallel with grep's sort path. This
    /// test simulates the worst case: most paths don't exist (Err from
    /// fs::metadata) and sort still completes successfully.
    #[test]
    fn sort_paths_by_mtime_desc_does_not_panic_on_missing_files() {
        // Mix of existing and non-existing paths in deliberately
        // non-monotonic order — pre-fix, the sort would call stat() at
        // least N log N times and any flakiness would trigger the panic.
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut paths: Vec<PathBuf> = Vec::new();
        for i in 0..30 {
            // Half exist, half don't.
            let path = if i % 2 == 0 {
                let p = dir.path().join(format!("real-{i}.rs"));
                fs::write(&p, format!("// {i}\n")).expect("write");
                p
            } else {
                dir.path().join(format!("missing-{i}.rs"))
            };
            paths.push(path);
        }

        // Run the sort many times to maximise the chance of catching any
        // residual non-determinism. Pre-fix: panic. Post-fix: stable.
        for _ in 0..50 {
            let mut copy = paths.clone();
            sort_paths_by_mtime_desc(&mut copy);
            assert_eq!(copy.len(), paths.len());
        }
    }

    /// Regression: v0.15.2 — sort_grep_matches_by_mtime_desc panicked under
    /// the same conditions as sort_paths_by_mtime_desc. See the
    /// sort_paths_... test above for the full rationale.
    #[test]
    fn sort_grep_matches_by_mtime_desc_does_not_panic_on_missing_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut matches: Vec<GrepMatch> = Vec::new();
        for i in 0..30 {
            let file = if i % 2 == 0 {
                let p = dir.path().join(format!("real-{i}.rs"));
                fs::write(&p, format!("// {i}\n")).expect("write");
                p
            } else {
                dir.path().join(format!("missing-{i}.rs"))
            };
            matches.push(GrepMatch {
                file,
                line: u32::try_from(i).unwrap_or(0),
                column: 0,
                line_text: format!("match {i}"),
                match_text: format!("match {i}"),
            });
        }

        for _ in 0..50 {
            let mut copy = matches.clone();
            sort_grep_matches_by_mtime_desc(&mut copy, dir.path());
            assert_eq!(copy.len(), matches.len());
        }
    }
}
