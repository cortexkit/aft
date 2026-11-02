//! Explicit patterns against another Git project use the same collectors and
//! ranking engines as the current project, but never its resident artifacts.
//! The borrowed postings must be checked first: a stale posting can otherwise
//! rule out a file whose new content matches the pattern.

use super::*;

pub(super) struct Corpus<'a> {
    pub index: Arc<SearchIndex>,
    pub generation: GenerationToken,
    semantic: &'a ReadOnlyArtifact<Arc<SemanticIndex>>,
    check: external_disk_check::DiskCheck,
    age: Option<Duration>,
    saved: bool,
    root: &'a Path,
    artifact_path: Option<&'a Path>,
}

/// Reuse the external query path's checked overlay, including its short reuse
/// window and resumable comparison. Both query and pattern requests must see
/// the same disk-validated copy and generation for cached rankings.
pub(super) fn checked_index(
    ctx: &AppContext,
    root: &Path,
    saved: &Arc<SearchIndex>,
    semantic: Option<&Arc<SemanticIndex>>,
    generation: &GenerationToken,
) -> (
    external_disk_check::CheckedIndex,
    Option<Duration>,
    GenerationToken,
) {
    let budgets = external_disk_check::budgets();
    let lookup = ctx.with_checked_overlays(|overlays| {
        overlays.lookup(
            root,
            generation.as_str(),
            saved,
            semantic,
            budgets.reuse_window,
        )
    });
    let (checked, age) = match lookup {
        external_disk_check::OverlayLookup::Reuse(checked, age) => (checked, Some(age)),
        lookup => {
            let previous = match lookup {
                external_disk_check::OverlayLookup::Resume(copy) => Some(copy),
                _ => None,
            };
            let checked = external_disk_check::check_against_disk(
                saved,
                previous.as_ref(),
                root,
                semantic.map(Arc::as_ref),
            );
            if !search_cancellation_requested() {
                crate::slog_debug!(
                    "external disk check of {}{}: {} files compared in {} ms, {} read in {} ms ({} not read, walk complete: {})",
                    root.display(),
                    if previous.is_some() { " (resumed)" } else { "" },
                    checked.check.files_examined,
                    checked.walk_time.as_millis(),
                    checked.check.reread,
                    checked.reread_time.as_millis(),
                    checked.check.not_reread,
                    checked.check.walk_complete,
                );
                ctx.with_checked_overlays(|overlays| {
                    overlays.remember(root, generation.as_str(), saved, semantic, checked.clone())
                });
            }
            (checked, None)
        }
    };
    let generation = checked.applied_digest.as_ref().map_or_else(
        || generation.clone(),
        |digest| GenerationToken::new_with_str(&format!("{}:disk:{digest}", generation.as_str())),
    );
    (checked, age, generation)
}

impl Corpus<'_> {
    pub(super) fn collect(
        &self,
        compiled: &pattern_compile::CompiledPattern,
        include_tests: bool,
    ) -> std::io::Result<crate::search_index::GrepFileCollection> {
        let mut collection = self.index.snapshot().collect_grep_matches_by_file(
            compiled,
            &PathFilters::default(),
            self.root,
            grep_path_exclusion(include_tests),
            regex_route::limits(),
            &regex_route::examine_priority,
            &regex_route::keep_past_line_limit,
        )?;
        // Even a completed examination of the postings cannot claim that the
        // pattern matched nothing if the walk or re-read left files unchecked.
        collection.examination_capped |= !self.check.verified();
        Ok(collection)
    }

    fn semantic_index(&self, ctx: &AppContext) -> Option<&SemanticIndex> {
        match self.semantic {
            ReadOnlyArtifact::Fresh(index)
            | ReadOnlyArtifact::Stale(crate::readonly_artifacts::ReadOnlyStale { index, .. })
                if semantic_fingerprint_matches_session(ctx, index) =>
            {
                Some(index)
            }
            _ => None,
        }
    }

    pub(super) fn semantic_results(
        &self,
        ctx: &AppContext,
        prose: &str,
        include_tests: bool,
        plan: &extensions::LanePlan<'_>,
    ) -> SplitSemantic {
        let Some(index) = self.semantic_index(ctx) else {
            return match self.semantic {
                ReadOnlyArtifact::Degraded(_) => SplitSemantic::unavailable(
                    "building", borrowed_semantic_loading_notice(0),
                ),
                _ => SplitSemantic::unavailable(
                    "unavailable",
                    "Semantic search is not available for the external root; lexical engine results follow.",
                ),
            };
        };
        if !plan.contains(SearchLaneKind::Semantic) {
            let mut semantic = SplitSemantic::unavailable("ready", "");
            semantic.gap = None;
            semantic.served = true;
            return semantic;
        }
        let vector = match embed_query_for_dimension(prose, ctx, Some(index.dimension())) {
            Ok(vector) => vector,
            Err(error) => return SplitSemantic::from_gap(semantic_embed_gap(&error)),
        };
        let mut results = index.search_filtered(
            &vector,
            SEMANTIC_ENUMERATION_LIMIT.saturating_add(1),
            |file| path_allowed_by_include_tests(file, self.root, include_tests),
        );
        results.retain(|result| result.file.is_file());
        let more_available = results.len() > SEMANTIC_ENUMERATION_LIMIT;
        results.truncate(SEMANTIC_ENUMERATION_LIMIT);
        rerank_semantic_candidates(&mut results, &query_shape::classify(prose), prose);
        SplitSemantic {
            results,
            more_available,
            status: "ready",
            gap: None,
            served: true,
            query_vector: Some(vector),
            not_ready: None,
            checkout_gaps: None,
        }
    }

    pub(super) fn cosines(
        &self,
        ctx: &AppContext,
        vector: &[f32],
        paths: &[PathBuf],
    ) -> HashMap<PathBuf, f32> {
        let Some(index) = self.semantic_index(ctx) else {
            return HashMap::new();
        };
        paths
            .iter()
            .filter_map(|path| {
                index
                    .search_filtered(vector, 1, |file| file == path.as_path())
                    .first()
                    .map(|result| (path.clone(), result.score))
            })
            .collect()
    }

    fn disclose(&self, response: &mut Response, semantic_served: bool) {
        if self.saved {
            disclose_checked_saved_index(
                response,
                self.root,
                self.artifact_path,
                &self.check,
                self.age,
                semantic_served,
            );
        } else if response.success {
            let Some(data) = response.data.as_object_mut() else {
                return;
            };
            let checked = self
                .check
                .files_examined
                .saturating_sub(self.check.not_reread);
            let coverage = if self.check.verified() {
                format!("checked all {checked} files on disk")
            } else {
                format!(
                    "reached {} files on disk and resolved {checked} before its time limit ({} not read; walk complete: {})",
                    self.check.files_examined, self.check.not_reread, self.check.walk_complete,
                )
            };
            append_reply_paragraph(data, &format!(
                "No AFT index is available for {}. The bounded lexical scan {coverage}; the pattern and query were searched over the text files admitted by that scan. Use grep with path {} for an exhaustive check.",
                self.root.display(), self.root.display(),
            ));
            data.insert("unindexed_scan".to_string(), self.check.to_json());
            if !self.check.verified() {
                data.insert("complete".to_string(), serde_json::json!(false));
            }
        }
        if let Some(data) = response.data.as_object_mut() {
            if data
                .get("engine_capped")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                append_reply_paragraph(data, &format!(
                    "The pattern examination was bounded; use grep with path {} for an exhaustive check.",
                    self.root.display(),
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle(
    req: &RawRequest,
    ctx: &AppContext,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    prose: Option<&str>,
    pattern: &str,
    compiled: &pattern_compile::CompiledPattern,
    root: &Path,
) -> Response {
    let config = ctx.config();
    let storage = config.cache_storage_dir();
    let source = ExternalReadinessSource::new(ctx, root, storage.as_deref());
    let mut readiness = extensions.sample_readiness(&extensions::Root::new(
        root,
        &source as &dyn extensions::ReadinessSource,
    ));
    if readiness.cancelled() || search_cancellation_requested() {
        return cancelled_search_response(req);
    }
    let artifacts = source.loaded().expect("external readiness loads artifacts");
    let semantic = match &artifacts.semantic {
        ReadOnlyArtifact::Fresh(index)
        | ReadOnlyArtifact::Stale(crate::readonly_artifacts::ReadOnlyStale { index, .. })
            if semantic_fingerprint_matches_session(ctx, index) =>
        {
            Some(index)
        }
        _ => None,
    };
    let saved = match &artifacts.search {
        ReadOnlyArtifact::Fresh(index)
        | ReadOnlyArtifact::Stale(crate::readonly_artifacts::ReadOnlyStale { index, .. }) => {
            Some(index)
        }
        ReadOnlyArtifact::Cancelled => return cancelled_search_response(req),
        _ => None,
    };
    let (checked, age, generation) = if let Some(saved) = saved {
        checked_index(ctx, root, saved, semantic, &artifacts.search_generation)
    } else {
        // Checking an empty index performs the same bounded, ignore-aware walk
        // and reads files into an in-memory copy. Nothing is saved to the other
        // project's cache, and both lanes can rank the files actually reached.
        let empty = SearchIndex::empty_ready_for_root(root);
        let checked = external_disk_check::check_against_disk(
            &Arc::new(empty),
            None,
            root,
            semantic.map(Arc::as_ref),
        );
        let generation = GenerationToken::new_with_str(&format!(
            "{}:walk:{}",
            artifacts.search_generation.as_str(),
            checked.applied_digest.as_deref().unwrap_or("empty"),
        ));
        (checked, None, generation)
    };
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }
    let corpus = Corpus {
        index: checked.index,
        generation,
        semantic: &artifacts.semantic,
        check: checked.check,
        age,
        saved: saved.is_some(),
        root,
        artifact_path: artifacts.search_artifact_path.as_deref(),
    };
    readiness.lexical_index = true;
    let include_tests = req
        .params
        .get("include_tests")
        .or_else(|| req.params.get("includeTests"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let _attribution = crate::search_b2::embed_counter::install(req.id.clone());
    let (plan, mut response) = if let Some(prose) = prose {
        let plan = split_query_plan(extensions, prose, &readiness);
        handle_split_search(
            req,
            ctx,
            page_request,
            extensions,
            plan,
            root,
            prose,
            pattern,
            compiled,
            include_tests,
            Some(&corpus),
        )
    } else {
        let plan = extensions.plan(
            &SearchShape::Regex,
            &crate::search_b2::lane_plan::no_query_facts(),
            &readiness,
        );
        let mut response = handle_grep_search(
            req,
            ctx,
            pattern,
            page_request.offset(),
            page_request.top_k(),
            &query_shape::classify(pattern),
            SearchMode::Regex,
            "external",
            Vec::new(),
            root,
            include_tests,
            page_request,
            extensions,
            &plan,
            true,
            Some(&corpus),
        );
        if let Some(data) = response.data.as_object_mut() {
            data.insert("query".to_string(), serde_json::json!(""));
            data.insert("pattern".to_string(), serde_json::json!(pattern));
        }
        (plan, response)
    };
    if response.success {
        let mut metadata = ExternalBorrowMetadata::default();
        match &artifacts.search {
            ReadOnlyArtifact::Stale(stale) => {
                metadata.record_drift(stale.drift_count, stale.ignore_rules_differ)
            }
            ReadOnlyArtifact::Degraded(degradation) => {
                metadata.degraded_reason = Some(degradation.reason)
            }
            _ => {}
        }
        if let ReadOnlyArtifact::Stale(stale) = &artifacts.semantic {
            metadata.record_drift(stale.drift_count, stale.ignore_rules_differ);
        }
        if let Some(data) = response.data.as_object_mut() {
            data.extend(
                external_response_extras(root, &metadata)
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            if let ReadOnlyArtifact::Degraded(_) = &artifacts.search {
                append_reply_paragraph(data, "Borrowed trigram index loading stopped at the interactive budget; the pattern and query used a bounded lexical scan instead.");
            }
        }
        let semantic_served = plan.contains(SearchLaneKind::Semantic);
        corpus.disclose(&mut response, semantic_served);
        if let (Some(data), Some(index)) =
            (response.data.as_object_mut(), corpus.semantic_index(ctx))
        {
            disclose_skipped_semantic_rows(data, index);
        }
        attach_search_execution_metadata(
            &mut response,
            &plan,
            crate::search_b2::embed_counter::read(&req.id),
        );
    }
    response
}
