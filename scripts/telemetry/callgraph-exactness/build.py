#!/usr/bin/env python3
"""Build a pinned source revision with only the exactness instrumentation overlaid."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import time

from child_process import child_process, terminal_failure

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
OLDEST_SUPPORTED = "06503f7fbf9e900fcc2dc9c78e26ff86d1d0d654"


def overlay_dead_code(source, sha):
    """Let the gate compare stored and dead-code rows one caller at a time."""
    layer = Path("crates/aft/src/data_layer")

    def needs(text, anchor, label):
        if text.count(anchor) != 1:
            raise ValueError(f"exactness {sha}: missing or ambiguous {label}: {anchor.strip()!r}; "
                             f"oldest supported crate-split revision is {OLDEST_SUPPORTED}")

    snapshot = source / layer / "dead_code_snapshot.rs"
    needs(snapshot.read_text() if snapshot.is_file() else "", "trait EntryPoints {",
          "crate-split dead-code snapshot interface")

    for filename, method in (("checkout.rs", "fn dead_code_files("),
                             ("graph_store.rs", "fn parse_of("),
                             ("graph_store.rs", "fn resolution_of(")):
        path = source / layer / filename
        needs(path.read_text() if path.is_file() else "", method, f"per-file read in {filename}")

    for name in ("dead_code/stream.rs", "dead_code/stream", "dead_code_graph/stream.rs"):
        local = REPO / layer / name
        dest = source / layer / name
        if local.is_dir():
            shutil.copytree(local, dest, dirs_exist_ok=True)
        else:
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(local, dest)

    dead = source / layer / "dead_code.rs"
    text = dead.read_text()
    if "mod stream;" not in text:
        dead.write_text(text + '\n#[cfg(feature = "test-hooks")]\npub(crate) mod stream;\n')

    graph = source / layer / "dead_code_graph.rs"
    text = graph.read_text()
    if "mod stream;" in text:
        return

    def graft(old, new, label):
        nonlocal text
        needs(text, old, label)
        text = text.replace(old, new, 1)

    # The comparison can discard each caller's decoded symbols. Leave the
    # archive's projection rules and the normal rollup's caching in place.
    marker = "use crate::data_layer::dead_code_snapshot::DEFAULT_EXPORT_MARKER_KIND;"
    graft(marker, marker + '\n\n#[cfg(feature = "test-hooks")]\nmod stream;',
          "dead-code graph module anchor")
    entry = """    pub(crate) fn project_file(
        &self,
        path: &str,
        classify: &dyn Fn(u32) -> SeedClass,
        keep_call_lines: bool,
    ) -> Result<FileProjection, String> {"""
    implementation = entry.replace("pub(crate) fn project_file(", "fn project_file_in(").replace(
        "        keep_call_lines: bool,", "        keep_call_lines: bool,\n        streaming: bool,")
    graft(entry, entry + "\n        self.project_file_in(path, classify, keep_call_lines, false)\n    }\n\n"
          + implementation,
          "rollup file projection entry point")
    graft('''go_initializers: (&*file.language == "go")
                .then(|| self.go_initializers(path, file, &rows))
                .flatten(),''', '''go_initializers: if &*file.language == "go" {
                self.go_initializers(path, file, &rows, streaming)?
            } else {
                None
            },''',
          "rollup Go projection call")
    graft("        rows: &FileRows,\n    ) -> Option<GoInitializerGroup> {",
          "        rows: &FileRows,\n        streaming: bool,\n    ) -> Result<Option<GoInitializerGroup>, String> {",
          "rollup Go projection signature")
    graft("if callers.is_empty() {\n            return None;", "if callers.is_empty() {\n            return Ok(None);",
          "rollup Go projection empty result")
    graft("let Some(target) = self.indexed(target_file) else {", """let scratch;
            let target = if streaming && target_file != path {
                let parse = self.source.parse(target_file)?;
                scratch = IndexedFile::build(target_file, &parse);
                &scratch
            } else if let Some(file) = self.indexed(target_file) {
                file
            } else {""", "rollup Go package file read")
    graft("let mut interned_symbols = BTreeMap::<&str, Arc<str>>::new();",
          "let mut interned_symbols = BTreeSet::<Arc<str>>::new();", "rollup Go symbol interner")
    graft("""let symbol = interned_symbols
                        .entry(&node.scoped_name)
                        .or_insert_with(|| Arc::from(&*node.scoped_name))
                        .clone();""", """let symbol = if let Some(symbol) = interned_symbols.get(&*node.scoped_name) {
                        Arc::clone(symbol)
                    } else {
                        let symbol = Arc::<str>::from(&*node.scoped_name);
                        interned_symbols.insert(Arc::clone(&symbol));
                        symbol
                    };""", "rollup Go symbol interning")
    graft("        Some(GoInitializerGroup {", "        Ok(Some(GoInitializerGroup {", "rollup Go projection result")
    graft("            targets,\n        })", "            targets,\n        }))", "rollup Go projection result end")
    graph.write_text(text)


def build(revision, output, profile):
    sha = subprocess.check_output(["git", "rev-parse", revision], cwd=REPO, text=True).strip()
    output.mkdir(parents=True, exist_ok=False)
    source = output / "source"
    source.mkdir()
    archive = subprocess.Popen(["git", "archive", sha], cwd=REPO, stdout=subprocess.PIPE)
    with tarfile.open(fileobj=archive.stdout, mode="r|*") as tar:
        tar.extractall(source, filter="data")
    if archive.wait():
        raise RuntimeError("git archive failed")
    overlay_dead_code(source, sha)
    for name in ("crates/aft/src/data_layer/checkout/exactness.rs", "crates/aft/examples/callgraph_exactness.rs"):
        path = source / name
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(REPO / name, path)
    checkout = source / "crates/aft/src/data_layer/checkout.rs"
    text = checkout.read_text()
    if "pub mod exactness;" not in text:
        text = text.replace("mod engines;", 'mod engines;\n#[cfg(feature = "test-hooks")]\npub mod exactness;', 1)
    checkout.write_text(text)
    graph = source / "crates/aft/src/data_layer/callgraph.rs"
    text = graph.read_text()
    if "flip_site_for_control" not in text:
        anchor = "file.site_dependencies = dependencies;"
        if text.count(anchor) != 1:
            raise ValueError("candidate needs explicit integration of the test-only flip_site_for_control hook")
        text = text.replace(anchor, anchor + '\n                        #[cfg(feature = "test-hooks")]\n                        super::checkout::exactness::flip_site_for_control(&path, &mut file);')
    graph.write_text(text)
    gate = source / "crates/aft/src/inspect/dead_code_gate.rs"
    text = gate.read_text()
    if "fn exactness_rows(" not in text:
        local = (REPO / "crates/aft/src/inspect/dead_code_gate.rs").read_text()
        helper = local.split("/// All dead-code finding rows,", 1)[1].split("/// Where two aggregates differ,", 1)[0]
        text += "\n/// All dead-code finding rows," + helper
    gate.write_text(text)
    manifest = source / "crates/aft/Cargo.toml"
    text = manifest.read_text()
    if 'name = "callgraph_exactness"' not in text:
        text += '\n[[example]]\nname = "callgraph_exactness"\nrequired-features = ["test-hooks"]\n'
    manifest.write_text(text)
    # A scratch source must never populate the shared warm Cargo target:
    # subsequent jobs could reuse its artifacts for a different source tree.
    target = output / "cargo-target"
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    command = ["cargo", "build", "--profile", profile, "-p", "agent-file-tools", "--example", "callgraph_exactness", "--features", "test-hooks"]
    started = time.monotonic()
    with (output / "build.log").open("w") as log, child_process(
        command, cwd=source, env=env, stdout=log, stderr=subprocess.STDOUT
    ) as process:
        while True:
            try:
                code = process.wait(timeout=60)
                break
            except subprocess.TimeoutExpired:
                print(f"exactness build {sha}: {time.monotonic() - started:.0f}s", flush=True)
                if time.monotonic() - started > 3600:
                    process.kill()
                    process.wait()
                    raise TimeoutError(f"exactness {sha}: cargo build exceeded 3600s")
    print("\n".join((output / "build.log").read_text().splitlines()[-20:]), flush=True)
    if code:
        raise subprocess.CalledProcessError(code, command)
    shutil.copyfile(target / profile / "examples/callgraph_exactness", output / "probe")
    (output / "probe").chmod(0o755)
    (output / "source-commit").write_text(sha + "\n")
    (output / "build.json").write_text(json.dumps({"source_commit": sha, "profile": profile,
        "command": command, "seconds": time.monotonic() - started,
        "probe_sha256": hashlib.sha256((output / "probe").read_bytes()).hexdigest(),
        "overlay": "exactness probe, bounded dead-code check and test-hooks integrations only"}, indent=2) + "\n")
    print(f"EXACTNESS BUILT {sha}: {output / 'probe'}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("revision")
    parser.add_argument("output", type=Path)
    parser.add_argument("--profile", choices=("release", "ci-storm"), default="ci-storm")
    args = parser.parse_args()
    try:
        build(args.revision, args.output.resolve(), args.profile)
    except Exception:
        terminal_failure()
