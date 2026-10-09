use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::resolver::{normalize_path, ResolvedImport, ResolvedModule};
use super::types::{
    ExportFact, ExportUsage, FileId, ImportKind, LivenessVerdict, OxcExportVerdict,
    OxcFileVerdicts, OxcReExportContext, ReExportKind, OXC_PROVENANCE,
};
use crate::inspect::entry_points::EXECUTABLE_ROOT_ALL_EXPORTS;
use crate::inspect::frameworks::{detected_decorator_frameworks, Framework};
use crate::inspect::scanners::dead_code::is_test_code_file;

#[derive(Debug, Clone)]
struct ExportState {
    fact: ExportFact,
    status: ExportStatus,
    also_reexported: Vec<ReExportContextRef>,
    reference_origins: ReferenceOrigins,
    /// Whether anything other than a reference inside the declaring file kept
    /// this export alive or uncertain (an import, a re-export, an entry
    /// point). When false, a `Used` export is used only by its own file: the
    /// symbol is live, but the `export` keyword serves nobody.
    used_through_export: bool,
}

/// Reason recorded when an export is kept alive by a reference inside its own
/// file.
const SAME_FILE_VALUE_REFERENCE_REASON: &str = "same_file_value_reference";

#[derive(Debug, Clone, Default)]
struct ReferenceOrigins {
    test_files: BTreeSet<String>,
    has_non_test: bool,
}

impl ReferenceOrigins {
    fn record(&mut self, origin: &ReferenceOrigin) {
        match origin {
            ReferenceOrigin::Test { file } => {
                self.test_files.insert(file.clone());
            }
            ReferenceOrigin::NonTest => {
                self.has_non_test = true;
            }
        }
    }

    fn has_references(&self) -> bool {
        self.has_non_test || !self.test_files.is_empty()
    }

    fn test_only_files(&self) -> Vec<String> {
        if self.has_non_test || self.test_files.is_empty() {
            Vec::new()
        } else {
            self.test_files.iter().cloned().collect()
        }
    }
}

#[derive(Debug, Clone)]
enum ReferenceOrigin {
    Test { file: String },
    NonTest,
}

#[derive(Debug, Clone)]
enum ExportStatus {
    Used(String),
    Uncertain(String),
    Unused,
}

impl ExportStatus {
    fn verdict(&self) -> (LivenessVerdict, String) {
        match self {
            Self::Used(reason) => (LivenessVerdict::Used, reason.clone()),
            Self::Uncertain(reason) => (LivenessVerdict::Uncertain, reason.clone()),
            Self::Unused => (LivenessVerdict::Unused, "no_references".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
struct ModuleState {
    path: PathBuf,
    exports: Vec<ExportState>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SymbolKey {
    file_id: FileId,
    name: String,
}

impl SymbolKey {
    fn new(file_id: FileId, name: impl Into<String>) -> Self {
        Self {
            file_id,
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReExportContextRef {
    file_id: FileId,
    line: u32,
    exported_name: String,
}

#[derive(Debug, Clone)]
struct NamedForward {
    source: FileId,
    imported_name: String,
    context: ReExportContextRef,
}

#[derive(Debug, Clone)]
struct StarForward {
    source: FileId,
    line: u32,
}

#[derive(Debug, Clone)]
struct NamespaceForward {
    source: FileId,
}

#[derive(Debug, Clone, Default)]
struct ForwardingMap {
    named: BTreeMap<SymbolKey, Vec<NamedForward>>,
    star: BTreeMap<usize, Vec<StarForward>>,
    namespace: BTreeMap<SymbolKey, Vec<NamespaceForward>>,
}

impl ForwardingMap {
    fn new(modules: &[ResolvedModule]) -> Self {
        let mut map = Self::default();
        for module in modules {
            let from = module.facts.file_id;
            for re_export in &module.re_exports {
                let Some(source) = re_export.target else {
                    continue;
                };
                match re_export.fact.kind {
                    ReExportKind::Named => {
                        let Some(exported_name) = re_export.fact.exported_name.clone() else {
                            continue;
                        };
                        let Some(imported_name) = re_export.fact.imported_name.clone() else {
                            continue;
                        };
                        map.named
                            .entry(SymbolKey::new(from, exported_name.clone()))
                            .or_default()
                            .push(NamedForward {
                                source,
                                imported_name,
                                context: ReExportContextRef {
                                    file_id: from,
                                    line: re_export.fact.line,
                                    exported_name,
                                },
                            });
                    }
                    ReExportKind::Star => {
                        map.star.entry(from.0).or_default().push(StarForward {
                            source,
                            line: re_export.fact.line,
                        });
                    }
                    ReExportKind::Namespace => {
                        let Some(exported_name) = re_export.fact.exported_name.clone() else {
                            continue;
                        };
                        map.namespace
                            .entry(SymbolKey::new(from, exported_name.clone()))
                            .or_default()
                            .push(NamespaceForward { source });
                    }
                }
            }
        }
        map
    }
}

#[derive(Debug, Clone, Default)]
struct ResolutionSet {
    canonical: BTreeSet<SymbolKey>,
    namespace_targets: BTreeSet<FileId>,
}

impl ResolutionSet {
    fn canonical(symbol: SymbolKey) -> Self {
        let mut set = Self::default();
        set.canonical.insert(symbol);
        set
    }

    fn namespace(target: FileId) -> Self {
        let mut set = Self::default();
        set.namespace_targets.insert(target);
        set
    }

    fn merge(&mut self, other: ResolutionSet) {
        self.canonical.extend(other.canonical);
        self.namespace_targets.extend(other.namespace_targets);
    }

    fn is_empty(&self) -> bool {
        self.canonical.is_empty() && self.namespace_targets.is_empty()
    }

    fn single_canonical(&self) -> Option<&SymbolKey> {
        if self.namespace_targets.is_empty() && self.canonical.len() == 1 {
            self.canonical.iter().next()
        } else {
            None
        }
    }
}

pub fn compute_verdicts(
    project_root: &Path,
    modules: &[ResolvedModule],
    entry_points: &BTreeSet<PathBuf>,
    public_api_files: &BTreeSet<PathBuf>,
    executable_root_exports: &BTreeMap<PathBuf, BTreeSet<String>>,
    entry_reachability: bool,
) -> Vec<OxcFileVerdicts> {
    let mut graph = GraphBuilder::new(
        project_root,
        modules,
        entry_points,
        public_api_files,
        executable_root_exports,
    );
    graph.apply_root_re_export_seeding();
    graph.apply_executable_file_root_seeding();
    graph.apply_decorator_entry_point_seeding();
    graph.record_reference_origins();
    if entry_reachability {
        // A same-file value reference proves that an exported helper is used
        // locally, even when the file itself is not reached from an entry point.
        // This does not enqueue the file, so entry reachability still controls
        // cross-file propagation.
        graph.apply_same_file_references();
        graph.apply_entry_reachability();
    } else {
        graph.apply_same_file_references();
        graph.apply_imports();
        graph.apply_dynamic_imports();
    }
    graph.into_verdicts(project_root)
}

struct GraphBuilder<'a> {
    modules: &'a [ResolvedModule],
    states: Vec<ModuleState>,
    root_modules: BTreeSet<usize>,
    export_root_modules: BTreeSet<usize>,
    executable_root_exports: BTreeMap<usize, BTreeSet<String>>,
    decorator_frameworks_by_module: BTreeMap<usize, BTreeSet<Framework>>,
    forwarding: ForwardingMap,
    reference_origins_by_module: Vec<ReferenceOrigin>,
}

impl<'a> GraphBuilder<'a> {
    fn new(
        project_root: &Path,
        modules: &'a [ResolvedModule],
        entry_points: &BTreeSet<PathBuf>,
        public_api_files: &BTreeSet<PathBuf>,
        executable_root_exports: &BTreeMap<PathBuf, BTreeSet<String>>,
    ) -> Self {
        let mut root_modules = BTreeSet::new();
        let mut export_root_modules = BTreeSet::new();
        let mut executable_roots_by_module = BTreeMap::new();
        let mut decorator_framework_cache = DecoratorFrameworkCache::new(project_root);
        let mut decorator_frameworks_by_module = BTreeMap::new();
        let states = modules
            .iter()
            .map(|module| {
                let exports = module
                    .facts
                    .exports
                    .iter()
                    .filter(|fact| export_fact_is_candidate(&module.facts.path, fact))
                    .cloned()
                    .map(|fact| ExportState {
                        fact,
                        status: ExportStatus::Unused,
                        also_reexported: Vec::new(),
                        reference_origins: ReferenceOrigins::default(),
                        used_through_export: false,
                    })
                    .collect::<Vec<_>>();

                let mut state = ModuleState {
                    path: module.facts.path.clone(),
                    exports,
                };
                let module_id = module.facts.file_id.0;
                let executable_exports = executable_root_exports.get(&module.facts.path);
                let exports_everything = public_api_files.contains(&module.facts.path)
                    || (entry_points.contains(&module.facts.path) && executable_exports.is_none());
                if exports_everything {
                    root_modules.insert(module_id);
                    export_root_modules.insert(module_id);
                    let origin = ReferenceOrigin::NonTest;
                    for export in &mut state.exports {
                        mark_used(export, "entry_point", &origin);
                    }
                }
                if let Some(exports) = executable_exports {
                    root_modules.insert(module_id);
                    executable_roots_by_module.insert(module_id, exports.clone());
                }
                let decorator_frameworks =
                    decorator_framework_cache.frameworks_for_file(&module.facts.path);
                if !decorator_frameworks.is_empty() {
                    decorator_frameworks_by_module.insert(module_id, decorator_frameworks);
                }
                state
            })
            .collect::<Vec<_>>();
        let forwarding = ForwardingMap::new(modules);
        let reference_origins_by_module = modules
            .iter()
            .map(|module| reference_origin_for_path(project_root, &module.facts.path))
            .collect();
        let mut graph = Self {
            modules,
            states,
            root_modules,
            export_root_modules,
            executable_root_exports: executable_roots_by_module,
            decorator_frameworks_by_module,
            forwarding,
            reference_origins_by_module,
        };
        graph.attach_reexport_contexts();
        graph
    }

    fn attach_reexport_contexts(&mut self) {
        let mut by_canonical: BTreeMap<SymbolKey, BTreeSet<ReExportContextRef>> = BTreeMap::new();

        for forwards in self.forwarding.named.values() {
            for forward in forwards {
                let mut visited = BTreeSet::new();
                let resolution =
                    self.resolve_export_name(forward.source, &forward.imported_name, &mut visited);
                for canonical in resolution.canonical {
                    by_canonical
                        .entry(canonical)
                        .or_default()
                        .insert(forward.context.clone());
                }
            }
        }

        for (from_file, stars) in &self.forwarding.star {
            for star in stars {
                let mut visited_files = BTreeSet::new();
                let visible =
                    self.visible_export_resolutions(star.source, false, &mut visited_files);
                for (exported_name, resolution) in visible {
                    for canonical in resolution.canonical {
                        by_canonical
                            .entry(canonical)
                            .or_default()
                            .insert(ReExportContextRef {
                                file_id: FileId(*from_file),
                                line: star.line,
                                exported_name: exported_name.clone(),
                            });
                    }
                }
            }
        }

        for (canonical, contexts) in by_canonical {
            if let Some(export) = self.export_state_mut(&canonical) {
                export.also_reexported = contexts.into_iter().collect();
            }
        }
    }

    fn record_reference_origins(&mut self) {
        for (idx, module) in self.modules.iter().enumerate() {
            let origin = self.reference_origin_for_module(idx);
            for import in &module.imports {
                let Some(target) = import.target else {
                    continue;
                };
                if !import_binding_is_used(module, import) {
                    continue;
                }
                match import.fact.kind {
                    ImportKind::Named => {
                        if let Some(name) = import.fact.imported_name.as_deref() {
                            let mut visited = BTreeSet::new();
                            self.record_imported_name_reference(
                                target,
                                name,
                                &origin,
                                &mut visited,
                            );
                        }
                    }
                    ImportKind::Default => {
                        let mut visited = BTreeSet::new();
                        self.record_imported_name_reference(
                            target,
                            "default",
                            &origin,
                            &mut visited,
                        );
                    }
                    ImportKind::Namespace => {
                        self.record_usage_reference(target, &import.fact.usage, &origin);
                    }
                    ImportKind::SideEffect => {}
                }
            }
            for dynamic in &module.dynamic_imports {
                if let Some(target) = dynamic.target {
                    self.record_usage_reference(target, &dynamic.fact.usage, &origin);
                }
            }
        }
    }

    fn record_usage_reference(
        &mut self,
        target: FileId,
        usage: &ExportUsage,
        origin: &ReferenceOrigin,
    ) {
        if usage.all {
            let visible = self.visible_export_resolutions(target, true, &mut BTreeSet::new());
            for resolution in visible.values() {
                self.record_resolution_reference_origin(resolution.clone(), origin);
            }
        }
        for name in &usage.names {
            self.record_imported_name_reference(target, name, origin, &mut BTreeSet::new());
        }
    }

    fn apply_usage(
        &mut self,
        target: FileId,
        usage: &ExportUsage,
        reason: &str,
        origin: &ReferenceOrigin,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        if usage.all {
            self.mark_all_uncertain_collect(
                target,
                reason,
                &mut BTreeSet::new(),
                newly_live_modules,
            );
        }
        for name in &usage.names {
            self.mark_imported_name_collect(
                target,
                name,
                reason,
                origin,
                &mut BTreeSet::new(),
                newly_live_modules,
            );
        }
    }

    fn record_imported_name_reference(
        &mut self,
        target: FileId,
        name: &str,
        origin: &ReferenceOrigin,
        visited: &mut BTreeSet<(usize, String)>,
    ) {
        let resolution = self.resolve_export_name(target, name, visited);
        self.record_resolution_reference_origin(resolution, origin);
    }

    fn record_resolution_reference_origin(
        &mut self,
        resolution: ResolutionSet,
        origin: &ReferenceOrigin,
    ) {
        let mut pending = vec![resolution];
        let mut namespaces = BTreeSet::new();
        while let Some(resolution) = pending.pop() {
            for canonical in resolution.canonical {
                if let Some(export) = self.export_state_mut(&canonical) {
                    export.reference_origins.record(origin);
                }
            }
            for target in resolution.namespace_targets {
                if namespaces.insert(target) {
                    pending.extend(
                        self.visible_export_resolutions(target, true, &mut BTreeSet::new())
                            .into_values(),
                    );
                }
            }
        }
    }

    fn apply_same_file_references(&mut self) {
        for idx in 0..self.modules.len() {
            self.apply_same_file_references_for_module(idx);
        }
    }

    fn apply_same_file_references_for_module(&mut self, idx: usize) {
        let mut newly_live_modules = BTreeSet::new();
        self.apply_same_file_references_for_module_collect(idx, &mut newly_live_modules);
    }

    fn apply_same_file_references_for_module_collect(
        &mut self,
        idx: usize,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        let Some(module) = self.modules.get(idx) else {
            return;
        };
        let origin = self.reference_origin_for_module(idx);
        if module.facts.same_file_value_references.is_empty() {
            return;
        }
        let Some(state) = self.states.get_mut(idx) else {
            return;
        };
        for export in &mut state.exports {
            let Some(local_name) = export.fact.local_name.as_deref() else {
                continue;
            };
            if module.facts.same_file_value_references.contains(local_name)
                && mark_used(export, SAME_FILE_VALUE_REFERENCE_REASON, &origin)
            {
                newly_live_modules.insert(idx);
            }
        }
    }

    fn apply_root_re_export_seeding(&mut self) {
        let roots = self.export_root_modules.iter().copied().collect::<Vec<_>>();
        for root in roots {
            let mut newly_live_modules = BTreeSet::new();
            let mut visited_files = BTreeSet::new();
            let visible = self.visible_export_resolutions(FileId(root), true, &mut visited_files);
            let origin = ReferenceOrigin::NonTest;
            for resolution in visible.values() {
                self.mark_resolution_used_or_uncertain(
                    resolution.clone(),
                    "entry_point",
                    &origin,
                    &mut newly_live_modules,
                );
            }
        }
    }

    fn apply_executable_file_root_seeding(&mut self) {
        let roots = self
            .executable_root_exports
            .iter()
            .map(|(file_id, exports)| (*file_id, exports.clone()))
            .collect::<Vec<_>>();
        let origin = ReferenceOrigin::NonTest;
        for (root, exports) in roots {
            let mut newly_live_modules = BTreeSet::new();
            if exports.contains(EXECUTABLE_ROOT_ALL_EXPORTS) {
                let mut visited_files = BTreeSet::new();
                let visible =
                    self.visible_export_resolutions(FileId(root), true, &mut visited_files);
                for resolution in visible.values() {
                    self.mark_resolution_used_or_uncertain(
                        resolution.clone(),
                        "entry_point",
                        &origin,
                        &mut newly_live_modules,
                    );
                }
                self.root_modules.extend(newly_live_modules);
                continue;
            }
            for export_name in exports {
                let mut visited = BTreeSet::new();
                let resolution = self.resolve_export_name(FileId(root), &export_name, &mut visited);
                if resolution.is_empty() {
                    continue;
                }
                self.mark_resolution_used_or_uncertain(
                    resolution,
                    "entry_point",
                    &origin,
                    &mut newly_live_modules,
                );
            }
            self.root_modules.extend(newly_live_modules);
        }
    }

    fn apply_decorator_entry_point_seeding(&mut self) {
        let roots = self
            .decorator_frameworks_by_module
            .iter()
            .map(|(file_id, frameworks)| (*file_id, frameworks.clone()))
            .collect::<Vec<_>>();
        let origin = ReferenceOrigin::NonTest;
        for (module_idx, frameworks) in roots {
            let Some(module) = self.modules.get(module_idx) else {
                continue;
            };
            let export_names = module
                .facts
                .exports
                .iter()
                .filter(|export| export_has_framework_decorator(export, module, &frameworks))
                .map(|export| export.name.as_symbol())
                .collect::<Vec<_>>();
            let mut newly_live_modules = BTreeSet::new();
            for export_name in export_names {
                let mut visited = BTreeSet::new();
                let resolution =
                    self.resolve_export_name(FileId(module_idx), &export_name, &mut visited);
                if resolution.is_empty() {
                    continue;
                }
                self.mark_resolution_used_or_uncertain(
                    resolution,
                    "entry_point_decorator",
                    &origin,
                    &mut newly_live_modules,
                );
            }
            self.root_modules.extend(newly_live_modules);
        }
    }

    fn apply_entry_reachability(&mut self) {
        let mut live_modules = self.root_modules.clone();
        let mut queue = live_modules.iter().copied().collect::<VecDeque<_>>();

        while let Some(module_idx) = queue.pop_front() {
            let mut newly_live_modules = BTreeSet::new();
            self.apply_same_file_references_for_module_collect(module_idx, &mut newly_live_modules);
            enqueue_newly_live_modules(newly_live_modules, &mut live_modules, &mut queue);

            let Some(module) = self.modules.get(module_idx).cloned() else {
                continue;
            };
            let origin = self.reference_origin_for_module(module_idx);

            for import in &module.imports {
                let Some(target) = import.target else {
                    continue;
                };
                if !matches!(import.fact.kind, ImportKind::SideEffect)
                    && !import_binding_is_used(&module, import)
                {
                    continue;
                }
                let mut newly_live_modules = BTreeSet::new();
                match import.fact.kind {
                    ImportKind::Named => {
                        if let Some(name) = import.fact.imported_name.as_deref() {
                            let mut visited = BTreeSet::new();
                            self.mark_imported_name_collect(
                                target,
                                name,
                                "import",
                                &origin,
                                &mut visited,
                                &mut newly_live_modules,
                            );
                        }
                    }
                    ImportKind::Default => {
                        let mut visited = BTreeSet::new();
                        self.mark_imported_name_collect(
                            target,
                            "default",
                            "import",
                            &origin,
                            &mut visited,
                            &mut newly_live_modules,
                        );
                    }
                    ImportKind::Namespace => {
                        self.apply_usage(
                            target,
                            &import.fact.usage,
                            "namespace_import",
                            &origin,
                            &mut newly_live_modules,
                        );
                    }
                    ImportKind::SideEffect => {}
                }
                enqueue_module(target, &mut live_modules, &mut queue);
                enqueue_newly_live_modules(newly_live_modules, &mut live_modules, &mut queue);
            }

            for dynamic in &module.dynamic_imports {
                if dynamic.fact.is_literal {
                    if let Some(target) = dynamic.target {
                        let mut newly_live_modules = BTreeSet::new();
                        self.apply_usage(
                            target,
                            &dynamic.fact.usage,
                            "dynamic_import",
                            &origin,
                            &mut newly_live_modules,
                        );
                        enqueue_module(target, &mut live_modules, &mut queue);
                        enqueue_newly_live_modules(
                            newly_live_modules,
                            &mut live_modules,
                            &mut queue,
                        );
                    }
                }
            }
        }
    }

    fn apply_imports(&mut self) {
        for (idx, module) in self.modules.iter().enumerate() {
            let origin = self.reference_origin_for_module(idx);
            for import in &module.imports {
                let Some(target) = import.target else {
                    continue;
                };
                if !import_binding_is_used(module, import) {
                    continue;
                }
                match import.fact.kind {
                    ImportKind::Named => {
                        if let Some(name) = import.fact.imported_name.as_deref() {
                            let mut visited = BTreeSet::new();
                            self.mark_imported_name(target, name, "import", &origin, &mut visited);
                        }
                    }
                    ImportKind::Default => {
                        let mut visited = BTreeSet::new();
                        self.mark_imported_name(target, "default", "import", &origin, &mut visited);
                    }
                    ImportKind::Namespace => {
                        self.apply_usage(
                            target,
                            &import.fact.usage,
                            "namespace_import",
                            &origin,
                            &mut BTreeSet::new(),
                        );
                    }
                    ImportKind::SideEffect => {}
                }
            }
        }
    }

    fn apply_dynamic_imports(&mut self) {
        for (idx, module) in self.modules.iter().enumerate() {
            let origin = self.reference_origin_for_module(idx);
            for dynamic in &module.dynamic_imports {
                if let Some(target) = dynamic.target {
                    self.apply_usage(
                        target,
                        &dynamic.fact.usage,
                        "dynamic_import",
                        &origin,
                        &mut BTreeSet::new(),
                    );
                }
            }
        }
    }

    fn mark_imported_name(
        &mut self,
        target: FileId,
        name: &str,
        reason: &str,
        origin: &ReferenceOrigin,
        visited: &mut BTreeSet<(usize, String)>,
    ) {
        let mut newly_live_modules = BTreeSet::new();
        self.mark_imported_name_collect(
            target,
            name,
            reason,
            origin,
            visited,
            &mut newly_live_modules,
        );
    }

    fn mark_imported_name_collect(
        &mut self,
        target: FileId,
        name: &str,
        reason: &str,
        origin: &ReferenceOrigin,
        visited: &mut BTreeSet<(usize, String)>,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        let resolution = self.resolve_export_name(target, name, visited);
        self.mark_resolution_used_or_uncertain(resolution, reason, origin, newly_live_modules);
    }

    fn mark_all_uncertain_collect(
        &mut self,
        target: FileId,
        reason: &str,
        visited: &mut BTreeSet<usize>,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        if !visited.insert(target.0) {
            return;
        }
        let mut visible_visited = BTreeSet::new();
        let visible = self.visible_export_resolutions(target, true, &mut visible_visited);
        for resolution in visible.values() {
            self.mark_resolution_uncertain_with_namespace_visited(
                resolution.clone(),
                reason,
                newly_live_modules,
                visited,
            );
        }
    }

    fn mark_resolution_used_or_uncertain(
        &mut self,
        resolution: ResolutionSet,
        reason: &str,
        origin: &ReferenceOrigin,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        if let Some(canonical) = resolution.single_canonical().cloned() {
            self.mark_canonical_used(&canonical, reason, origin, newly_live_modules);
            return;
        }
        self.mark_resolution_uncertain(resolution, reason, newly_live_modules);
    }

    fn mark_resolution_uncertain(
        &mut self,
        resolution: ResolutionSet,
        reason: &str,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        let mut namespace_visited = BTreeSet::new();
        self.mark_resolution_uncertain_with_namespace_visited(
            resolution,
            reason,
            newly_live_modules,
            &mut namespace_visited,
        );
    }

    fn mark_resolution_uncertain_with_namespace_visited(
        &mut self,
        resolution: ResolutionSet,
        reason: &str,
        newly_live_modules: &mut BTreeSet<usize>,
        namespace_visited: &mut BTreeSet<usize>,
    ) {
        for canonical in resolution.canonical {
            self.mark_canonical_uncertain(&canonical, reason, newly_live_modules);
        }
        for target in resolution.namespace_targets {
            self.mark_all_uncertain_collect(
                target,
                "namespace_import",
                namespace_visited,
                newly_live_modules,
            );
        }
    }

    fn mark_canonical_used(
        &mut self,
        canonical: &SymbolKey,
        reason: &str,
        origin: &ReferenceOrigin,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        if let Some(export) = self.export_state_mut(canonical) {
            if mark_used(export, reason, origin) {
                newly_live_modules.insert(canonical.file_id.0);
            }
        }
    }

    fn mark_canonical_uncertain(
        &mut self,
        canonical: &SymbolKey,
        reason: &str,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        if let Some(export) = self.export_state_mut(canonical) {
            if mark_uncertain(export, reason) {
                newly_live_modules.insert(canonical.file_id.0);
            }
        }
    }

    fn resolve_export_name(
        &self,
        target: FileId,
        name: &str,
        visited: &mut BTreeSet<(usize, String)>,
    ) -> ResolutionSet {
        let key = SymbolKey::new(target, name.to_string());
        if !visited.insert((target.0, name.to_string())) {
            return ResolutionSet::default();
        }

        if self.has_local_export(&key) {
            return ResolutionSet::canonical(key);
        }

        let mut resolution = ResolutionSet::default();
        if let Some(forwards) = self.forwarding.named.get(&key) {
            for forward in forwards {
                resolution.merge(self.resolve_export_name(
                    forward.source,
                    &forward.imported_name,
                    visited,
                ));
            }
        }

        if let Some(forwards) = self.forwarding.namespace.get(&key) {
            for forward in forwards {
                resolution.merge(ResolutionSet::namespace(forward.source));
            }
        }

        if name != "default" {
            if let Some(stars) = self.forwarding.star.get(&target.0) {
                for star in stars {
                    resolution.merge(self.resolve_export_name(star.source, name, visited));
                }
            }
        }

        resolution
    }

    fn visible_export_resolutions(
        &self,
        target: FileId,
        include_default: bool,
        visited_files: &mut BTreeSet<usize>,
    ) -> BTreeMap<String, ResolutionSet> {
        if !visited_files.insert(target.0) {
            return BTreeMap::new();
        }

        let mut visible = BTreeMap::new();
        let mut explicit_names = BTreeSet::new();

        if let Some(state) = self.states.get(target.0) {
            for export in &state.exports {
                let name = export.fact.name.as_symbol();
                if !include_default && name == "default" {
                    continue;
                }
                explicit_names.insert(name.clone());
                visible.insert(
                    name.clone(),
                    ResolutionSet::canonical(SymbolKey::new(target, name)),
                );
            }
        }

        for (key, forwards) in self.forwarding.named.range(
            SymbolKey::new(target, String::new())..=SymbolKey::new(target, char::MAX.to_string()),
        ) {
            if key.file_id != target {
                continue;
            }
            if !include_default && key.name == "default" {
                continue;
            }
            explicit_names.insert(key.name.clone());
            let mut resolution = ResolutionSet::default();
            for forward in forwards {
                let mut visited = BTreeSet::new();
                resolution.merge(self.resolve_export_name(
                    forward.source,
                    &forward.imported_name,
                    &mut visited,
                ));
            }
            if !resolution.is_empty() {
                visible
                    .entry(key.name.clone())
                    .or_insert_with(ResolutionSet::default)
                    .merge(resolution);
            }
        }

        for (key, forwards) in self.forwarding.namespace.range(
            SymbolKey::new(target, String::new())..=SymbolKey::new(target, char::MAX.to_string()),
        ) {
            if key.file_id != target {
                continue;
            }
            if !include_default && key.name == "default" {
                continue;
            }
            explicit_names.insert(key.name.clone());
            let mut resolution = ResolutionSet::default();
            for forward in forwards {
                resolution.merge(ResolutionSet::namespace(forward.source));
            }
            visible
                .entry(key.name.clone())
                .or_insert_with(ResolutionSet::default)
                .merge(resolution);
        }

        if let Some(stars) = self.forwarding.star.get(&target.0) {
            for star in stars {
                let star_visible =
                    self.visible_export_resolutions(star.source, false, visited_files);
                for (name, resolution) in star_visible {
                    if explicit_names.contains(&name) {
                        continue;
                    }
                    visible
                        .entry(name)
                        .or_insert_with(ResolutionSet::default)
                        .merge(resolution);
                }
            }
        }

        visible
    }

    fn has_local_export(&self, key: &SymbolKey) -> bool {
        self.states.get(key.file_id.0).is_some_and(|state| {
            state
                .exports
                .iter()
                .any(|export| export.fact.name.matches_str(&key.name))
        })
    }

    fn reference_origin_for_module(&self, idx: usize) -> ReferenceOrigin {
        self.reference_origins_by_module
            .get(idx)
            .cloned()
            .unwrap_or(ReferenceOrigin::NonTest)
    }

    fn export_state_mut(&mut self, key: &SymbolKey) -> Option<&mut ExportState> {
        self.states.get_mut(key.file_id.0).and_then(|state| {
            state
                .exports
                .iter_mut()
                .find(|export| export.fact.name.matches_str(&key.name))
        })
    }

    fn into_verdicts(self, project_root: &Path) -> Vec<OxcFileVerdicts> {
        let paths_by_file_id = self
            .states
            .iter()
            .map(|state| state.path.clone())
            .collect::<Vec<_>>();
        self.states
            .into_iter()
            .map(|state| {
                let relative_file = relative_string(project_root, &state.path);
                let mut seen = BTreeSet::new();
                let exports = state
                    .exports
                    .into_iter()
                    .filter(|export| seen.insert(export.fact.name.as_symbol()))
                    .map(|export| {
                        let (verdict, reason) = export.status.verdict();
                        // Type-only exports are left out: a type used in its own
                        // file usually appears in an exported signature, where
                        // it must stay exported for callers to name it (unused-
                        // export checkers such as fallow keep those too).
                        let only_same_file_references = verdict == LivenessVerdict::Used
                            && !export.used_through_export
                            && !matches!(export.fact.kind.as_str(), "type" | "interface");
                        OxcExportVerdict {
                            symbol: export.fact.name.as_symbol(),
                            kind: export.fact.kind,
                            line: export.fact.line,
                            verdict,
                            reason,
                            provenance: OXC_PROVENANCE.to_string(),
                            has_references: export.reference_origins.has_references(),
                            test_only_reference_files: export.reference_origins.test_only_files(),
                            only_same_file_references,
                            also_reexported: export
                                .also_reexported
                                .into_iter()
                                .filter_map(|context| {
                                    let file = paths_by_file_id
                                        .get(context.file_id.0)
                                        .map(|path| relative_string(project_root, path))?;
                                    Some(OxcReExportContext {
                                        file,
                                        line: context.line,
                                        exported_name: context.exported_name,
                                    })
                                })
                                .collect(),
                        }
                    })
                    .collect::<Vec<_>>();
                OxcFileVerdicts {
                    file: state.path,
                    relative_file,
                    exports,
                }
            })
            .collect()
    }
}

fn enqueue_newly_live_modules(
    newly_live_modules: BTreeSet<usize>,
    live_modules: &mut BTreeSet<usize>,
    queue: &mut VecDeque<usize>,
) {
    for module_idx in newly_live_modules {
        enqueue_module_idx(module_idx, live_modules, queue);
    }
}

fn enqueue_module(target: FileId, live_modules: &mut BTreeSet<usize>, queue: &mut VecDeque<usize>) {
    enqueue_module_idx(target.0, live_modules, queue);
}

fn enqueue_module_idx(
    module_idx: usize,
    live_modules: &mut BTreeSet<usize>,
    queue: &mut VecDeque<usize>,
) {
    if live_modules.insert(module_idx) {
        queue.push_back(module_idx);
    }
}

fn import_binding_is_used(module: &ResolvedModule, import: &ResolvedImport) -> bool {
    match import.fact.kind {
        ImportKind::SideEffect => false,
        ImportKind::Named | ImportKind::Default | ImportKind::Namespace => import
            .fact
            .local_name
            .as_ref()
            .is_some_and(|local| module.facts.used_import_bindings.contains(local)),
    }
}

fn export_has_framework_decorator(
    export: &ExportFact,
    module: &ResolvedModule,
    frameworks: &BTreeSet<Framework>,
) -> bool {
    export
        .decorators
        .iter()
        .any(|decorator| decorator_matches_framework(module, &decorator.segments, frameworks))
}

fn export_fact_is_candidate(path: &Path, fact: &ExportFact) -> bool {
    !(fact.declared && is_declaration_file(path))
}

fn is_declaration_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts")
        })
}

fn decorator_matches_framework(
    module: &ResolvedModule,
    segments: &[String],
    frameworks: &BTreeSet<Framework>,
) -> bool {
    match segments {
        [local] => module.imports.iter().any(|import| {
            if import.fact.local_name.as_deref() != Some(local.as_str()) {
                return false;
            }
            let Some(imported_name) = decorator_imported_name(import) else {
                return false;
            };
            frameworks
                .iter()
                .any(|framework| framework.allows_decorator(&import.fact.source, imported_name))
        }),
        [namespace, member] => module.imports.iter().any(|import| {
            matches!(import.fact.kind, ImportKind::Namespace)
                && import.fact.local_name.as_deref() == Some(namespace.as_str())
                && frameworks
                    .iter()
                    .any(|framework| framework.allows_decorator(&import.fact.source, member))
        }),
        _ => false,
    }
}

fn decorator_imported_name(import: &ResolvedImport) -> Option<&str> {
    match import.fact.kind {
        ImportKind::Named => import.fact.imported_name.as_deref(),
        ImportKind::Default => Some("default"),
        ImportKind::Namespace | ImportKind::SideEffect => None,
    }
}

struct DecoratorFrameworkCache {
    project_root: PathBuf,
    by_start_dir: BTreeMap<PathBuf, BTreeSet<Framework>>,
    by_manifest: BTreeMap<PathBuf, BTreeSet<Framework>>,
}

impl DecoratorFrameworkCache {
    fn new(project_root: &Path) -> Self {
        Self {
            project_root: normalize_path(project_root),
            by_start_dir: BTreeMap::new(),
            by_manifest: BTreeMap::new(),
        }
    }

    fn frameworks_for_file(&mut self, file: &Path) -> BTreeSet<Framework> {
        let file = normalize_path(file);
        let start_dir = file
            .parent()
            .map(normalize_path)
            .unwrap_or_else(|| self.project_root.clone());
        if let Some(frameworks) = self.by_start_dir.get(&start_dir) {
            return frameworks.clone();
        }
        let frameworks = if start_dir.starts_with(&self.project_root) {
            self.nearest_manifest_frameworks(&start_dir)
        } else {
            BTreeSet::new()
        };
        self.by_start_dir.insert(start_dir, frameworks.clone());
        frameworks
    }

    fn nearest_manifest_frameworks(&mut self, start_dir: &Path) -> BTreeSet<Framework> {
        let mut dir = start_dir.to_path_buf();
        loop {
            let manifest = dir.join("package.json");
            if manifest.is_file() {
                return self.frameworks_for_manifest(&manifest);
            }
            if dir == self.project_root || !dir.pop() || !dir.starts_with(&self.project_root) {
                return BTreeSet::new();
            }
        }
    }

    fn frameworks_for_manifest(&mut self, manifest: &Path) -> BTreeSet<Framework> {
        let manifest = normalize_path(manifest);
        if let Some(frameworks) = self.by_manifest.get(&manifest) {
            return frameworks.clone();
        }
        let frameworks = fs::read(&manifest)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .map(|manifest| detected_decorator_frameworks(&manifest))
            .unwrap_or_default();
        self.by_manifest.insert(manifest, frameworks.clone());
        frameworks
    }
}

fn mark_used(export: &mut ExportState, reason: &str, origin: &ReferenceOrigin) -> bool {
    export.reference_origins.record(origin);
    if reason != SAME_FILE_VALUE_REFERENCE_REASON {
        export.used_through_export = true;
    }
    match export.status {
        ExportStatus::Used(_) => false,
        ExportStatus::Uncertain(_) => {
            export.status = ExportStatus::Used(reason.to_string());
            false
        }
        ExportStatus::Unused => {
            export.status = ExportStatus::Used(reason.to_string());
            true
        }
    }
}

fn mark_uncertain(export: &mut ExportState, reason: &str) -> bool {
    // Uncertainty comes from another module (a wildcard or namespace use), so
    // the export binding itself may be in use.
    export.used_through_export = true;
    if matches!(export.status, ExportStatus::Unused) {
        export.status = ExportStatus::Uncertain(reason.to_string());
        true
    } else {
        false
    }
}

fn reference_origin_for_path(project_root: &Path, path: &Path) -> ReferenceOrigin {
    let relative = relative_string(project_root, path);
    if is_test_code_file(&relative, &BTreeSet::new()) {
        ReferenceOrigin::Test { file: relative }
    } else {
        ReferenceOrigin::NonTest
    }
}

fn relative_string(project_root: &Path, path: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(project_root) {
        return path_components_string(relative);
    }

    let canonical_root =
        fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canonical_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical_path
        .strip_prefix(&canonical_root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn path_components_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
