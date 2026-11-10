//! Which TypeScript a project has installed, and which language server can
//! serve it.
//!
//! TypeScript 6 and earlier ship `lib/tsserver.js`, which
//! `typescript-language-server` loads. TypeScript 7 and later are the native
//! (Go) compiler: they ship no `tsserver.js` at all, but the compiler binary
//! serves LSP itself with `tsc --lsp --stdio`. The binary lives in a
//! per-platform optional dependency, `@typescript/typescript-<os>-<cpu>`, next
//! to the `typescript` package.
//!
//! The decision is always made from what is installed in `node_modules`, never
//! from a lockfile: after a branch switch without an install, the lockfile
//! names what should be installed while `node_modules` holds what actually
//! runs, and starting the server that matches the lockfile would fail against
//! the installed compiler.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::environmental::TS_NATIVE_NO_TSSERVER;

thread_local! {
    /// How many installed TypeScript package.json reads this thread has attempted.
    /// Per thread so a test can count its own lookups while other tests run in
    /// parallel.
    static PACKAGE_JSON_READS: Cell<u64> = const { Cell::new(0) };
}

/// Attempted TypeScript package.json reads on this thread.
#[cfg(test)]
pub(crate) fn package_json_reads_on_this_thread() -> u64 {
    PACKAGE_JSON_READS.with(Cell::get)
}

/// The `serverInfo.name` the native TypeScript language server reports in its
/// `initialize` response.
pub(crate) const NATIVE_SERVER_INFO_NAME: &str = "typescript-go";

/// Arguments that put the native compiler binary into language-server mode.
/// The mode is not listed in `tsc --help`, but `tsc --lsp --help` documents it.
pub(crate) const NATIVE_SERVER_ARGS: [&str; 2] = ["--lsp", "--stdio"];

/// An installed TypeScript package. Project lookup starts at the nearest
/// `node_modules/typescript/package.json`; a shim's SDK can be a dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectTypeScript {
    /// The package.json `version`, or "unknown" when it has none.
    pub(crate) version: String,
    /// The installed package directory.
    pub(crate) package_dir: PathBuf,
}

impl ProjectTypeScript {
    /// Leading integer of the version ("7.0.2" and "7.1.0-dev.1" are both 7).
    pub(crate) fn major(&self) -> Option<u64> {
        let digits: String = self
            .version
            .trim()
            .trim_start_matches('v')
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    }

    /// TypeScript 7 and later are the native (Go) compiler, which ships no
    /// tsserver.js for typescript-language-server to load.
    pub(crate) fn is_native_compiler(&self) -> bool {
        self.major().is_some_and(|major| major >= 7)
    }

    /// The lib directory containing the tsserver.js that typescript-language-server
    /// loads, in this package or a runtime dependency aliased to `typescript` by a
    /// shim package.
    pub(crate) fn tsserver_lib_dir(&self) -> Option<PathBuf> {
        find_tsserver_lib_dir(&self.package_dir, &mut HashSet::new())
    }

    /// Read an installed TypeScript package at `package_dir`. `None` when its
    /// package.json cannot be read.
    pub(crate) fn read(package_dir: &Path) -> Option<Self> {
        PACKAGE_JSON_READS.with(|reads| reads.set(reads.get() + 1));
        let path = package_dir.join("package.json");
        super::applicability_cache::observe_path(&path);
        let bytes = std::fs::read(path).ok()?;
        let version = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|json| {
                json.get("version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unknown".into());
        Some(Self {
            version,
            package_dir: package_dir.to_path_buf(),
        })
    }
}

/// Limit file-system work for deeply nested alias chains.
const MAX_TSSERVER_PACKAGES: usize = 16;

fn find_tsserver_lib_dir(package_dir: &Path, visited: &mut HashSet<PathBuf>) -> Option<PathBuf> {
    if visited.len() >= MAX_TSSERVER_PACKAGES {
        return None;
    }
    let real_dir = crate::inspect::job::canonicalize_normalized(package_dir);
    if !visited.insert(real_dir.clone()) {
        return None;
    }
    let lib = package_dir.join("lib");
    let tsserver = lib.join("tsserver.js");
    super::applicability_cache::observe_path(&tsserver);
    if tsserver.is_file() {
        return Some(lib);
    }
    let manifest = real_dir.join("package.json");
    super::applicability_cache::observe_path(&manifest);
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest).ok()?).ok()?;
    ["dependencies", "optionalDependencies"]
        .into_iter()
        .filter_map(|section| json.get(section)?.as_object())
        .flatten()
        .find_map(|(name, spec)| {
            let spec = spec.as_str()?;
            if (spec != "npm:typescript" && !spec.starts_with("npm:typescript@"))
                || visited.len() >= MAX_TSSERVER_PACKAGES
            {
                return None;
            }
            let dependency = resolve_node_package(&real_dir, name)?;
            find_tsserver_lib_dir(&dependency, visited)
        })
}

/// Resolve an installed package from its importer's real path, as Node does.
fn resolve_node_package(real_dir: &Path, name: &str) -> Option<PathBuf> {
    if Path::new(name)
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    for ancestor in real_dir.ancestors() {
        // Node looks in `<dir>/node_modules` for every ancestor, except that
        // a directory that is itself named node_modules is searched directly.
        let modules = if ancestor
            .file_name()
            .is_some_and(|name| name == "node_modules")
        {
            ancestor.to_path_buf()
        } else {
            ancestor.join("node_modules")
        };
        let package = modules.join(name);
        let manifest = package.join("package.json");
        super::applicability_cache::observe_path(&manifest);
        if manifest.is_file() {
            return Some(package);
        }
    }
    None
}

/// Find the project's own TypeScript package for `source_file`, walking up to
/// `project_root`. It looks for `package.json` rather than `lib/*.js` because
/// TypeScript 7 ships none of the JavaScript files the SDK probe expects.
pub(crate) fn find_project_typescript_package(
    source_file: &Path,
    project_root: &Path,
) -> Option<ProjectTypeScript> {
    let mut directory = source_file.parent()?;
    loop {
        if let Some(found) =
            ProjectTypeScript::read(&directory.join("node_modules").join("typescript"))
        {
            return Some(found);
        }
        if directory == project_root {
            return None;
        }
        let parent = directory.parent()?;
        if !parent.starts_with(project_root) {
            return None;
        }
        directory = parent;
    }
}

/// Whether any `package.json` that can supply TypeScript to the files a
/// TypeScript server serves declares a `typescript` dependency (of any kind).
/// Used only after the server reported that it found no TypeScript and AFT
/// found no installed one, to tell "dependencies not installed" apart from
/// "nothing here uses TypeScript".
///
/// Two places are searched:
/// - the directories from `source_file` up to `boundary`, the same walk
///   [`find_project_typescript_package`] makes, so a declaring manifest
///   above the server root counts;
/// - every package under `server_root` that this server serves. One server
///   serves every package below its root that sees the same installed
///   TypeScript (see [`shared_typescript_server_root`]), so a server whose
///   root has no TypeScript also serves the packages that have none
///   installed. A package with its own `node_modules/typescript` has its own
///   server and is skipped, together with everything under it.
///
/// When nothing declares TypeScript, the files are outside every TypeScript
/// project, for example stray scripts at a workspace root whose packages
/// each install their own TypeScript. They are never served with a
/// package's TypeScript: the lookup walks up from a file, never into a
/// sibling package. An unreadable `package.json` counts as declaring, so an
/// unparseable manifest never turns into a confident "no project".
pub(crate) fn typescript_dependency_declared(
    source_file: &Path,
    server_root: &Path,
    boundary: &Path,
) -> bool {
    if let Some(mut directory) = source_file.parent() {
        loop {
            if package_json_declares_typescript(&directory.join("package.json")) {
                return true;
            }
            if directory == boundary {
                break;
            }
            let Some(parent) = directory.parent() else {
                break;
            };
            if !parent.starts_with(boundary) {
                break;
            }
            directory = parent;
        }
    }
    let walker = ignore::WalkBuilder::new(server_root)
        .same_file_system(true)
        .standard_filters(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            if !is_dir {
                return entry.file_name() == "package.json";
            }
            if crate::lsp::roots::skip_in_server_walk(
                entry.file_name().to_string_lossy().as_ref(),
                entry.depth(),
            ) {
                return false;
            }
            // A package with its own TypeScript is served by its own server.
            entry.depth() == 0
                || !entry
                    .path()
                    .join("node_modules")
                    .join("typescript")
                    .join("package.json")
                    .is_file()
        })
        .build();
    walker.flatten().any(|entry| {
        entry.file_type().is_some_and(|kind| kind.is_file())
            && package_json_declares_typescript(entry.path())
    })
}

fn package_json_declares_typescript(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        // An unreadable manifest cannot prove the project has no TypeScript;
        // keep the install advice rather than a confident "no project".
        return true;
    };
    [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ]
    .iter()
    .any(|section| {
        json.get(section)
            .and_then(|deps| deps.get("typescript"))
            .is_some()
    })
}

/// The project's TypeScript for `source_file`, served from `server_root`.
///
/// The walk is bounded by the configured project root when the file is inside
/// it, and by the server root otherwise, the same boundary the SDK lookup for
/// `typescript-language-server` uses. Paths are canonicalized first so a
/// symlinked temp or checkout path still compares equal to its root.
pub(crate) fn project_typescript_for(
    source_file: &Path,
    server_root: &Path,
    project_root: Option<&Path>,
) -> Option<ProjectTypeScript> {
    let source_file = crate::inspect::job::canonicalize_normalized(source_file);
    let server_root = crate::inspect::job::canonicalize_normalized(server_root);
    let project_root = project_root.map(crate::inspect::job::canonicalize_normalized);
    let boundary = project_root
        .as_deref()
        .filter(|root| source_file.starts_with(root))
        .unwrap_or(&server_root);
    find_project_typescript_package(&source_file, boundary)
}

/// A TypeScript 7+ installation chosen for a file, and its native binary when
/// it resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeTypeScript {
    pub(crate) project: ProjectTypeScript,
    pub(crate) binary: Option<PathBuf>,
}

/// Entries kept before the memo is emptied and refilled. Each entry is one
/// source directory, so this covers far more directories than a large
/// monorepo has while keeping memory bounded.
const SELECTION_MEMO_CAPACITY: usize = 16_384;

/// (directory the ancestor walk starts in, configured project root, hash of
/// the root-marker file names the server definition uses to find its
/// workspace root).
type SelectionKey = (PathBuf, Option<PathBuf>, u64, u64, Option<u64>);

static SELECTION_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Memo of [`native_typescript_for`] results.
///
/// The applicability walk and scoped inspect ask which TypeScript server
/// serves every TS/JS file they visit, and each answer costs a canonicalize,
/// an ancestor walk with a `package.json` read attempt per level, and the
/// platform-binary lookup. Every file in one directory gets the same answer,
/// so the answer is memoized per directory and repeated lookups are one hash
/// probe. It is emptied on configure and when a watched path that can change
/// the answer changes (see [`invalidate_typescript_selection`]).
fn selection_memo() -> &'static parking_lot::Mutex<HashMap<SelectionKey, Option<NativeTypeScript>>>
{
    static MEMO: OnceLock<parking_lot::Mutex<HashMap<SelectionKey, Option<NativeTypeScript>>>> =
        OnceLock::new();
    MEMO.get_or_init(Default::default)
}

/// The TypeScript 7+ installation that serves `source_file`, or `None` when
/// its nearest installed TypeScript is not the native compiler (or there is
/// none). `server_root` is only called when the answer is not memoized; it
/// bounds the walk when the file is outside `project_root`. `markers` is a
/// fingerprint of the root markers that `server_root` uses, so definitions
/// with different markers do not share answers.
pub(crate) fn native_typescript_for(
    source_file: &Path,
    project_root: Option<&Path>,
    markers: u64,
    mode: super::roots::RootPathMode,
    server_root: impl FnOnce() -> Option<PathBuf>,
) -> Option<NativeTypeScript> {
    let start = source_file.parent()?;
    let key = (
        start.to_path_buf(),
        project_root.map(Path::to_path_buf),
        markers,
        SELECTION_GENERATION.load(std::sync::atomic::Ordering::Acquire),
        super::applicability_cache::capture_id(),
    );
    // Each complete applicability scan rediscovers its external dependencies;
    // files in the same scan still share the per-directory selection memo.
    if let Some(memoized) = selection_memo().lock().get(&key) {
        return memoized.clone();
    }
    // Computed without the lock so parallel lookups in other directories do
    // not wait on this one's file-system work.
    let answer = server_root().and_then(|root| {
        let project = match mode {
            super::roots::RootPathMode::Resolve => {
                project_typescript_for(source_file, &root, project_root)
            }
            super::roots::RootPathMode::WalkEntry => {
                let boundary = project_root
                    .filter(|root| source_file.starts_with(root))
                    .unwrap_or(&root);
                find_project_typescript_package(source_file, boundary)
            }
        }
        .filter(ProjectTypeScript::is_native_compiler)?;
        let binary = resolve_native_binary(&project).ok();
        Some(NativeTypeScript { project, binary })
    });
    let mut memo = selection_memo().lock();
    if memo.len() >= SELECTION_MEMO_CAPACITY {
        memo.clear();
    }
    memo.insert(key, answer.clone());
    answer
}

/// Forget every memoized TypeScript server choice.
pub(crate) fn clear_typescript_selection() {
    // An in-flight lookup may finish after this clear. Its old-generation key
    // must not become a reusable answer about the changed installation.
    SELECTION_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    selection_memo().lock().clear();
    shared_root_memo().lock().clear();
}

/// Maps (nearest TypeScript root marker directory, project root) to the root
/// the shared TypeScript server for that directory runs at. Emptied together
/// with the selection memo.
type SharedRootMemo = parking_lot::Mutex<HashMap<(PathBuf, PathBuf, u64, Option<u64>), PathBuf>>;

fn shared_root_memo() -> &'static SharedRootMemo {
    static MEMO: OnceLock<SharedRootMemo> = OnceLock::new();
    MEMO.get_or_init(Default::default)
}

/// The workspace root for the `typescript-language-server` that serves the
/// package whose nearest root marker directory is `nearest`.
///
/// One server can serve many packages: tsserver resolves each opened file's
/// own tsconfig, and runs several configured projects in one process. What it
/// cannot do is run two TypeScript versions, because the server loads one
/// `tsserver.js`. So the root moves up from `nearest` through every ancestor
/// with a root marker that sees the same installed TypeScript version, and
/// stops at the first ancestor that sees a different one (or none, when
/// `nearest` has one). Packages that each install the same version then
/// share the server at the project root instead of starting one per package.
pub(crate) fn shared_typescript_server_root(
    nearest: &Path,
    project_root: &Path,
    has_marker: impl Fn(&Path) -> bool,
    mode: super::roots::RootPathMode,
) -> PathBuf {
    let project_root = mode.resolve(project_root);
    if !nearest.starts_with(&project_root) {
        return nearest.to_path_buf();
    }
    let key = (
        nearest.to_path_buf(),
        project_root.clone(),
        SELECTION_GENERATION.load(std::sync::atomic::Ordering::Acquire),
        super::applicability_cache::capture_id(),
    );
    if let Some(root) = shared_root_memo().lock().get(&key) {
        return root.clone();
    }
    // Identifies the TypeScript SDK a directory's files would load: its version,
    // or its canonical package path when the version is unreadable, so different
    // unreadable installations never count as the same one.
    let typescript_of = |dir: &Path| {
        find_project_typescript_package(&dir.join("package.json"), &project_root).map(
            |mut found| {
                if let Some(lib) = found.tsserver_lib_dir() {
                    let package_dir = lib.parent().unwrap().to_path_buf();
                    found = ProjectTypeScript::read(&package_dir).unwrap_or(ProjectTypeScript {
                        version: "unknown".into(),
                        package_dir,
                    });
                }
                if found.major().is_none() {
                    crate::inspect::job::canonicalize_normalized(&found.package_dir)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    found.version
                }
            },
        )
    };
    let own = typescript_of(nearest);
    let mut chosen = nearest.to_path_buf();
    let mut current = nearest.parent();
    while let Some(dir) = current {
        if !dir.starts_with(&project_root) {
            break;
        }
        if has_marker(dir) {
            if typescript_of(dir) != own {
                break;
            }
            chosen = dir.to_path_buf();
        }
        if dir == project_root {
            break;
        }
        current = dir.parent();
    }
    let mut memo = shared_root_memo().lock();
    if memo.len() >= SELECTION_MEMO_CAPACITY {
        memo.clear();
    }
    memo.insert(key, chosen.clone());
    chosen
}

/// Whether a change to `path` can change which TypeScript server a file gets:
/// the installed TypeScript or its platform package, the `.bin/tsc` link an
/// install rewrites, a lockfile or `package.json` (installs and branch
/// switches change these), or a root marker that moves a server root.
///
/// The file watcher drops most paths inside `node_modules`, so after an
/// install the lockfile or `package.json` change is usually the signal that
/// arrives; configure clears the memo as well.
pub(crate) fn path_affects_typescript_selection(path: &Path) -> bool {
    const NAMES: &[&str] = &[
        "package.json",
        "tsconfig.json",
        "jsconfig.json",
        "bun.lock",
        "bun.lockb",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
    ];
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| NAMES.contains(&name))
    {
        return true;
    }
    let components: Vec<_> = path
        .components()
        .map(|component| component.as_os_str())
        .collect();
    components.windows(2).any(|pair| {
        pair[0] == "node_modules" && (pair[1] == "typescript" || pair[1] == "@typescript")
    }) || components.windows(3).any(|triple| {
        triple[0] == "node_modules"
            && triple[1] == ".bin"
            && Path::new(triple[2])
                .file_stem()
                .is_some_and(|stem| stem == "tsc")
    })
}

/// Forget the memoized choices when any of `paths` can change them.
pub(crate) fn invalidate_typescript_selection<'a>(paths: impl IntoIterator<Item = &'a Path>) {
    if paths.into_iter().any(path_affects_typescript_selection) {
        clear_typescript_selection();
    }
}

/// The npm name of the platform package that carries the native compiler for
/// the running machine, in Node's `process.platform`/`process.arch` spelling
/// (`@typescript/typescript-darwin-arm64`). `None` on a platform Node has no
/// name for, where no such package can exist.
pub(crate) fn native_platform_package() -> Option<String> {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "solaris" | "illumos" => "sunos",
        os @ ("linux" | "freebsd" | "netbsd" | "openbsd" | "aix") => os,
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        "loongarch64" => "loong64",
        "powerpc64" => "ppc64",
        "mips64" if cfg!(target_endian = "little") => "mips64el",
        arch @ ("arm" | "riscv64" | "s390x") => arch,
        _ => return None,
    };
    Some(format!("@typescript/typescript-{platform}-{arch}"))
}

/// Locate the native language-server binary for a TypeScript 7+ package.
///
/// This mirrors the package's own `lib/getExePath.js`: resolve the platform
/// package the way Node resolves a bare specifier from inside the
/// `typescript` package, then use its `lib/tsc` (`tsc.exe` on Windows).
/// Resolution starts from the package's real path because Bun's isolated
/// linker and pnpm install `node_modules/typescript` as a symlink into a
/// store, and the platform package sits next to the real directory, not the
/// link. AFT starts the binary directly rather than through
/// `node_modules/.bin/tsc`, a `#!/usr/bin/env node` wrapper that machines
/// with only Bun cannot run.
///
/// The error names the missing package and where it was looked for.
pub(crate) fn resolve_native_binary(project: &ProjectTypeScript) -> Result<PathBuf, String> {
    let Some(platform_package) = native_platform_package() else {
        return Err(format!(
            "no native TypeScript binary is published for this platform ({}-{})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    };
    let real_dir = crate::inspect::job::canonicalize_normalized(&project.package_dir);
    let binary_name = if cfg!(windows) { "tsc.exe" } else { "tsc" };
    if let Some(package) = resolve_node_package(&real_dir, &platform_package) {
        let binary = package.join("lib").join(binary_name);
        super::applicability_cache::observe_path(&binary);
        if binary.is_file() {
            return Ok(binary);
        }
        return Err(format!(
            "{platform_package} is installed at {} but has no lib/{binary_name}",
            package.display()
        ));
    }
    Err(format!(
        "{platform_package} (the optional dependency that carries the native compiler) was not found from {}; the install skipped optional dependencies or this platform is unsupported",
        real_dir.display()
    ))
}

/// Named gap for a TypeScript 7+ project whose native language server could
/// not be started. Nothing was spawned: `typescript-language-server` is known
/// to fail on the native compiler, and installing dependencies with LSP
/// auto-install cannot add a tsserver.js that TypeScript 7 never ships.
pub(crate) fn native_server_unavailable_reason(project: &ProjectTypeScript, cause: &str) -> String {
    format!(
        "TypeScript unavailable: this project uses TypeScript {} (native compiler) at {}, {TS_NATIVE_NO_TSSERVER}, and AFT could not start its native language server: {cause}. Run the project's tsc --noEmit for type errors.",
        project.version,
        project.package_dir.display(),
    )
}

/// Named gap for a native server binary whose `initialize` response did not
/// identify it as `typescript-go`. AFT shuts it down rather than trust
/// diagnostics from a program it cannot vouch for.
pub(crate) fn native_server_misidentified_reason(
    project: Option<&ProjectTypeScript>,
    package_dir: &Path,
    binary: &Path,
    reported: Option<&str>,
) -> String {
    let version = project.map_or("unknown", |project| project.version.as_str());
    let reported = reported.map_or_else(
        || "reported no serverInfo name".to_string(),
        |name| format!("identified itself as \"{name}\""),
    );
    format!(
        "TypeScript unavailable: the native language server {} for TypeScript {version} at {} {reported} rather than {NATIVE_SERVER_INFO_NAME}, so AFT stopped it instead of trusting its diagnostics. Run the project's tsc --noEmit for type errors.",
        binary.display(),
        package_dir.display(),
    )
}

/// Named gap for a project TypeScript that neither server can serve: its
/// version is unreadable, or it is TypeScript 6 or earlier without the
/// `lib/tsserver.js` typescript-language-server needs. Nothing was spawned.
pub(crate) fn unservable_typescript_reason(project: &ProjectTypeScript) -> String {
    if project.major().is_none() {
        return format!(
            "TypeScript unavailable: the project's TypeScript at {} reports version \"{}\", which AFT cannot read, so it cannot tell whether typescript-language-server (TypeScript 6 and earlier) or the native language server (TypeScript 7 and later) serves it. Check the installed package; run the project's tsc --noEmit for type errors.",
            project.package_dir.display(),
            project.version,
        );
    }
    format!(
        "TypeScript unavailable: the project's TypeScript {} at {} has no lib/tsserver.js, so typescript-language-server can't serve it, and TypeScript before 7 has no native language server. The installation looks incomplete: reinstall the project's dependencies, or run the project's tsc --noEmit for type errors.",
        project.version,
        project.package_dir.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn major_version_is_the_leading_integer() {
        let ts = |version: &str| ProjectTypeScript {
            version: version.into(),
            package_dir: PathBuf::from("/repo/node_modules/typescript"),
        };
        assert_eq!(ts("7.0.2").major(), Some(7));
        assert_eq!(ts("7.1.0-dev.20260929.1").major(), Some(7));
        assert_eq!(ts("5.9.3").major(), Some(5));
        assert_eq!(ts("unknown").major(), None);
        assert!(ts("10.0.0").is_native_compiler());
        assert!(!ts("6.0.1").is_native_compiler());
        assert!(!ts("next").is_native_compiler());
    }

    #[test]
    fn platform_package_uses_node_spelling() {
        let name = native_platform_package().expect("test hosts have a published platform");
        assert!(name.starts_with("@typescript/typescript-"), "{name}");
        assert!(
            !name.contains("macos") && !name.contains("aarch64"),
            "{name}"
        );
        assert!(
            !name.contains("x86_64") && !name.contains("windows"),
            "{name}"
        );
    }

    fn native_install(modules: &Path, version: &str) -> PathBuf {
        let package_dir = modules.join("typescript");
        write(
            &package_dir.join("package.json"),
            &format!(r#"{{"name":"typescript","version":"{version}"}}"#),
        );
        let platform = modules.join(native_platform_package().unwrap());
        write(&platform.join("package.json"), "{}");
        let binary = platform
            .join("lib")
            .join(if cfg!(windows) { "tsc.exe" } else { "tsc" });
        write(&binary, "");
        package_dir
    }

    fn shim_install(package_dir: &Path, alias: &str) {
        write(
            &package_dir.join("package.json"),
            &serde_json::json!({
                "version": "6.0.2",
                "dependencies": {alias: "npm:typescript@^6"}
            })
            .to_string(),
        );
    }

    #[test]
    fn tsserver_is_found_in_a_normal_typescript_package() {
        let temp = tempfile::tempdir().unwrap();
        let package_dir = temp.path().join("node_modules/typescript");
        write(
            &package_dir.join("package.json"),
            r#"{"name":"typescript","version":"6.0.2"}"#,
        );
        let lib = package_dir.join("lib");
        write(&lib.join("tsserver.js"), "");
        let project = ProjectTypeScript::read(&package_dir).unwrap();
        assert_eq!(project.tsserver_lib_dir(), Some(lib));
    }

    #[test]
    fn tsserver_is_found_in_a_hoisted_alias_dependency() {
        let temp = tempfile::tempdir().unwrap();
        let modules =
            crate::inspect::job::canonicalize_normalized(temp.path()).join("node_modules");
        let package_dir = modules.join("typescript");
        shim_install(&package_dir, "@typescript/old");
        let lib = modules.join("@typescript/old/lib");
        write(
            &lib.parent().unwrap().join("package.json"),
            r#"{"version":"6.0.3"}"#,
        );
        write(&lib.join("tsserver.js"), "");
        let project = ProjectTypeScript::read(&package_dir).unwrap();
        assert_eq!(project.tsserver_lib_dir(), Some(lib));
        assert_eq!(project.version, "6.0.2");
    }

    #[test]
    fn tsserver_is_found_in_a_versionless_alias_dependency() {
        let temp = tempfile::tempdir().unwrap();
        let modules =
            crate::inspect::job::canonicalize_normalized(temp.path()).join("node_modules");
        let package_dir = modules.join("typescript");
        write(
            &package_dir.join("package.json"),
            r#"{"version":"6.0.2","dependencies":{"legacy":"npm:typescript"}}"#,
        );
        let lib = modules.join("legacy/lib");
        write(
            &lib.parent().unwrap().join("package.json"),
            r#"{"version":"6.0.3"}"#,
        );
        write(&lib.join("tsserver.js"), "");
        let project = ProjectTypeScript::read(&package_dir).unwrap();
        assert_eq!(project.tsserver_lib_dir(), Some(lib));
    }

    #[cfg(unix)]
    #[test]
    fn tsserver_is_resolved_from_the_real_path_of_a_linked_shim() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        for (store, app) in [(".bun", "bun-app"), (".pnpm", "pnpm-app")] {
            let store_modules = root
                .join("node_modules")
                .join(store)
                .join("@typescript+typescript6@6.0.2/node_modules");
            let real_package = store_modules.join("@typescript/typescript6");
            shim_install(&real_package, "@typescript/old");
            let lib = store_modules.join("@typescript/old/lib");
            write(
                &lib.parent().unwrap().join("package.json"),
                r#"{"version":"6.0.2"}"#,
            );
            write(&lib.join("tsserver.js"), "");
            let project_modules = root.join(app).join("node_modules");
            std::fs::create_dir_all(&project_modules).unwrap();
            std::os::unix::fs::symlink(&real_package, project_modules.join("typescript")).unwrap();

            let project = ProjectTypeScript::read(&project_modules.join("typescript")).unwrap();
            assert_eq!(project.tsserver_lib_dir(), Some(lib));
        }
    }

    #[test]
    fn tsserver_alias_cycles_terminate() {
        let temp = tempfile::tempdir().unwrap();
        let package_dir = temp.path().join("node_modules/typescript");
        shim_install(&package_dir, "typescript");
        let project = ProjectTypeScript::read(&package_dir).unwrap();
        assert_eq!(project.tsserver_lib_dir(), None);
    }

    #[test]
    fn tsserver_only_follows_typescript_aliases_with_package_names() {
        let temp = tempfile::tempdir().unwrap();
        let modules = temp.path().join("node_modules");
        let package_dir = modules.join("typescript");
        let dependency = modules.join("legacy");
        write(&dependency.join("package.json"), "{}");
        write(&dependency.join("lib/tsserver.js"), "");
        for spec in ["npm:typescript-other@^6", "^6"] {
            write(
                &package_dir.join("package.json"),
                &serde_json::json!({"version": "6.0.2", "dependencies": {"legacy": spec}})
                    .to_string(),
            );
            let project = ProjectTypeScript::read(&package_dir).unwrap();
            assert_eq!(project.tsserver_lib_dir(), None);
        }
        shim_install(&package_dir, "../node_modules/legacy");
        let project = ProjectTypeScript::read(&package_dir).unwrap();
        assert_eq!(project.tsserver_lib_dir(), None);
    }

    #[test]
    fn tsserver_alias_chains_have_a_package_limit() {
        let temp = tempfile::tempdir().unwrap();
        let modules =
            crate::inspect::job::canonicalize_normalized(temp.path()).join("node_modules");
        let package_dir = modules.join("typescript");
        shim_install(&package_dir, "legacy0");
        for index in 0..MAX_TSSERVER_PACKAGES {
            shim_install(
                &modules.join(format!("legacy{index}")),
                &format!("legacy{}", index + 1),
            );
        }
        let far_lib = modules.join(format!("legacy{MAX_TSSERVER_PACKAGES}/lib"));
        write(&far_lib.parent().unwrap().join("package.json"), "{}");
        write(&far_lib.join("tsserver.js"), "");
        let project = ProjectTypeScript::read(&package_dir).unwrap();
        assert_eq!(project.tsserver_lib_dir(), None);

        let near_lib = modules.join("legacy1/lib");
        write(&near_lib.join("tsserver.js"), "");
        assert_eq!(project.tsserver_lib_dir(), Some(near_lib));
    }

    #[test]
    fn native_binary_is_found_next_to_a_hoisted_package() {
        let temp = tempfile::tempdir().unwrap();
        let modules = temp.path().join("node_modules");
        let package_dir = native_install(&modules, "7.0.2");
        let project = ProjectTypeScript::read(&package_dir).unwrap();
        let binary = resolve_native_binary(&project).unwrap();
        assert!(binary.ends_with(
            Path::new(&native_platform_package().unwrap())
                .join("lib")
                .join(if cfg!(windows) { "tsc.exe" } else { "tsc" })
        ));
    }

    /// Bun's isolated linker and pnpm link `node_modules/typescript` into a
    /// store; the platform package is a sibling of the real directory only.
    #[cfg(unix)]
    #[test]
    fn native_binary_is_resolved_from_the_real_path_of_a_linked_package() {
        let temp = tempfile::tempdir().unwrap();
        let store_modules = temp
            .path()
            .join("node_modules")
            .join(".bun")
            .join("typescript@7.0.2")
            .join("node_modules");
        let real_package = native_install(&store_modules, "7.0.2");
        let project_modules = temp.path().join("app").join("node_modules");
        std::fs::create_dir_all(&project_modules).unwrap();
        std::os::unix::fs::symlink(&real_package, project_modules.join("typescript")).unwrap();

        let project = ProjectTypeScript::read(&project_modules.join("typescript")).unwrap();
        let binary = resolve_native_binary(&project).unwrap();
        assert!(
            binary.starts_with(crate::inspect::job::canonicalize_normalized(&store_modules)),
            "{}",
            binary.display()
        );
    }

    #[test]
    fn missing_platform_package_names_the_package_and_where_it_looked() {
        let temp = tempfile::tempdir().unwrap();
        let package_dir = temp.path().join("node_modules").join("typescript");
        write(
            &package_dir.join("package.json"),
            r#"{"name":"typescript","version":"7.0.2"}"#,
        );
        let project = ProjectTypeScript::read(&package_dir).unwrap();
        let cause = resolve_native_binary(&project).unwrap_err();
        assert!(
            cause.contains(&native_platform_package().unwrap()),
            "{cause}"
        );
        assert!(cause.contains("was not found from"), "{cause}");
        let reason = native_server_unavailable_reason(&project, &cause);
        assert!(
            reason.starts_with(
                "TypeScript unavailable: this project uses TypeScript 7.0.2 (native compiler)"
            ),
            "{reason}"
        );
        assert!(!reason.contains("bun install"), "{reason}");
        assert!(!reason.contains("auto-install"), "{reason}");
    }

    #[test]
    fn unservable_reasons_name_the_version_and_path() {
        let unreadable = ProjectTypeScript {
            version: "unknown".into(),
            package_dir: PathBuf::from("/repo/node_modules/typescript"),
        };
        let reason = unservable_typescript_reason(&unreadable);
        assert!(reason.contains("reports version \"unknown\""), "{reason}");
        assert!(reason.contains("/repo/node_modules/typescript"), "{reason}");

        let broken = ProjectTypeScript {
            version: "5.9.3".into(),
            ..unreadable
        };
        let reason = unservable_typescript_reason(&broken);
        assert!(
            reason.contains(
                "TypeScript 5.9.3 at /repo/node_modules/typescript has no lib/tsserver.js"
            ),
            "{reason}"
        );
    }

    #[test]
    fn detection_walks_from_the_file_and_stops_at_the_project_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        native_install(&root.join("node_modules"), "7.0.2");
        let file = root
            .join("packages")
            .join("app")
            .join("src")
            .join("index.ts");
        write(&file, "");
        let found = project_typescript_for(&file, &root.join("packages").join("app"), Some(&root))
            .expect("hoisted TypeScript above the server root but inside the project");
        assert_eq!(found.version, "7.0.2");

        // A nearer install wins over the hoisted one.
        let nearer = root
            .join("packages")
            .join("app")
            .join("node_modules")
            .join("typescript");
        write(&nearer.join("package.json"), r#"{"version":"5.9.3"}"#);
        let found =
            project_typescript_for(&file, &root.join("packages").join("app"), Some(&root)).unwrap();
        assert_eq!(found.version, "5.9.3");

        // Nothing above the project root is considered.
        native_install(&temp.path().join("node_modules"), "7.0.2");
        let outside = temp.path().join("other").join("src").join("index.ts");
        write(&outside, "");
        let project = temp.path().join("other");
        assert_eq!(
            project_typescript_for(&outside, &project, Some(&project)),
            None
        );
    }

    /// The per-package Bun workspace: every package installs its own
    /// TypeScript, the root declares none. Root scripts are outside every
    /// TypeScript project, and a package's declaration is not borrowed for
    /// them because that package has its own server.
    #[test]
    fn root_scripts_of_a_per_package_workspace_declare_no_typescript() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        write(
            &root.join("package.json"),
            r#"{"private":true,"workspaces":["packages/*"],"devDependencies":{"@biomejs/biome":"^2.5.1"}}"#,
        );
        let script = root.join("scripts").join("build.ts");
        write(&script, "");
        let plugin = root.join("packages").join("plugin");
        write(
            &plugin.join("package.json"),
            r#"{"devDependencies":{"typescript":"^5.9.3"}}"#,
        );
        write(&plugin.join("tsconfig.json"), "{}");
        write(
            &plugin
                .join("node_modules")
                .join("typescript")
                .join("package.json"),
            r#"{"version":"5.9.3"}"#,
        );
        assert!(!typescript_dependency_declared(&script, &root, &root));

        // A package that declares TypeScript but has none installed shares
        // the root's server (both see no TypeScript), so installing is the
        // remedy for that server even when its first file is a root script.
        write(
            &root.join("packages").join("fresh").join("package.json"),
            r#"{"dependencies":{"typescript":"5.9.3"}}"#,
        );
        assert!(typescript_dependency_declared(&script, &root, &root));
    }

    #[test]
    fn a_declaring_or_unreadable_manifest_on_the_walk_counts_as_declared() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let script = root.join("tools").join("gen.ts");
        write(&script, "");
        write(&root.join("package.json"), r#"{"name":"plain"}"#);
        assert!(!typescript_dependency_declared(
            &script,
            &root.join("tools"),
            &root
        ));
        // Declared above the server root, inside the project: install helps.
        write(
            &root.join("package.json"),
            r#"{"peerDependencies":{"typescript":"*"}}"#,
        );
        assert!(typescript_dependency_declared(
            &script,
            &root.join("tools"),
            &root
        ));
        // An unparseable manifest never yields a confident "no project".
        write(&root.join("package.json"), "{ not json");
        assert!(typescript_dependency_declared(
            &script,
            &root.join("tools"),
            &root
        ));
    }
}
