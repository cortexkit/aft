#!/usr/bin/env python3
"""Refuse Cargo and npm dependencies that resolve outside this repository."""

from __future__ import annotations

import argparse
import glob
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from typing import Any

SKIP_DIRS = {".git", "node_modules", "target"}
NPM_DEPENDENCY_TABLES = ("dependencies", "devDependencies", "peerDependencies", "optionalDependencies")


def repository_manifests(root: Path) -> list[Path]:
    manifests: list[Path] = []
    for current, directories, files in os.walk(root, topdown=True, followlinks=False):
        directories[:] = sorted(name for name in directories if name not in SKIP_DIRS)
        for name in ("Cargo.toml", "package.json"):
            if name in files:
                manifests.append(Path(current) / name)
    return sorted(manifests)


def inside_repository(path: Path, root: Path) -> bool:
    try:
        path.resolve(strict=False).relative_to(root.resolve(strict=False))
        return True
    except (OSError, ValueError, RuntimeError):
        return False


def short_error(result: subprocess.CompletedProcess[str]) -> str:
    text = " ".join((result.stderr or result.stdout).split())
    return text[:400] or f"command exited {result.returncode} without a diagnostic"


def cargo_env() -> dict[str, str]:
    """Cargo's environment without a compiler wrapper.

    Reading metadata only asks rustc for its version, and a configured wrapper
    such as sccache may not be installed yet where this gate runs (CI runs it
    before the build cache is set up), which makes cargo fail before reading
    any manifest.
    """
    env = dict(os.environ)
    for name in ("RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"):
        env[name] = ""
    return env


def cargo_workspace_root(manifest: Path) -> tuple[Path | None, str | None]:
    command = [
        "cargo",
        "locate-project",
        "--workspace",
        "--manifest-path",
        str(manifest),
    ]
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, check=False, env=cargo_env()
        )
    except OSError as error:
        return None, str(error)
    if result.returncode:
        return None, short_error(result)
    try:
        root_manifest = Path(json.loads(result.stdout)["root"]).resolve(strict=True)
    except (KeyError, json.JSONDecodeError, OSError, RuntimeError) as error:
        return None, f"invalid cargo locate-project response: {error}"
    return root_manifest, None


def cargo_metadata(manifest: Path) -> tuple[dict[str, Any] | None, str | None]:
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--offline",
        "--manifest-path",
        str(manifest),
    ]
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, check=False, env=cargo_env()
        )
    except OSError as error:
        return None, str(error)

    # Hosted CI can have a fresh Cargo cache. Retry there without --offline,
    # while keeping --locked so the check never rewrites a committed lockfile.
    if result.returncode and os.environ.get("CI", "").lower() in {"1", "true", "yes"}:
        online_command = [part for part in command if part != "--offline"]
        try:
            result = subprocess.run(
                online_command, capture_output=True, text=True, check=False, env=cargo_env()
            )
        except OSError as error:
            return None, str(error)
    if result.returncode:
        return None, short_error(result)
    try:
        metadata = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        return None, f"cargo metadata returned invalid JSON: {error}"
    if not isinstance(metadata, dict) or not isinstance(metadata.get("packages"), list):
        return None, "cargo metadata response has no package list"
    return metadata, None


def check_root(root: Path) -> tuple[list[str], list[str], list[str], int, int, int]:
    root = root.resolve(strict=True)
    manifests = repository_manifests(root)
    violations: list[str] = []
    unchecked: list[str] = []
    errors: list[str] = []
    checked_references = 0
    cargo_roots: dict[Path, Path] = {}

    def check_reference(manifest: Path, table: str, dependency: str, raw_path: str) -> None:
        nonlocal checked_references
        checked_references += 1
        try:
            resolved = (manifest.parent / raw_path).resolve(strict=False)
        except (OSError, RuntimeError) as error:
            violations.append(
                f"{manifest.relative_to(root)}: table={table} dependency={dependency} "
                f"cannot resolve {raw_path!r}: {error}"
            )
            return
        if not inside_repository(resolved, root):
            violations.append(
                f"{manifest.relative_to(root)}: table={table} dependency={dependency} resolved={resolved}"
            )

    def check_workspace_globs(manifest: Path, patterns: Any) -> None:
        nonlocal checked_references
        if not isinstance(patterns, list):
            return
        for pattern in patterns:
            if not isinstance(pattern, str) or pattern.startswith("!"):
                continue
            checked_references += 1
            paths = [manifest.parent / pattern]
            paths.extend(Path(item) for item in glob.glob(str(manifest.parent / pattern), recursive=True))
            outside: Path | None = None
            for candidate in paths:
                try:
                    resolved = candidate.resolve(strict=False)
                except (OSError, RuntimeError) as error:
                    violations.append(
                        f"{manifest.relative_to(root)}: table=[workspaces] dependency={pattern} "
                        f"cannot resolve path: {error}"
                    )
                    break
                if not inside_repository(resolved, root):
                    outside = resolved
                    break
            if outside is not None:
                violations.append(
                    f"{manifest.relative_to(root)}: table=[workspaces] dependency={pattern} resolved={outside}"
                )

    for manifest in manifests:
        if manifest.name == "Cargo.toml":
            workspace_manifest, error = cargo_workspace_root(manifest)
            if error is not None:
                relative = manifest.relative_to(root).as_posix()
                message = f"{relative}: cargo locate-project failed: {error}"
                # Test-input crates (call-graph fixtures and the store-diet
                # corpus) are parsed as source, never built, and sit inside
                # the repository's workspace without being members of it.
                if relative.startswith(
                    ("crates/aft/tests/fixtures/", "crates/aft/tests/store_diet/corpus/")
                ):
                    unchecked.append(message)
                else:
                    errors.append(message)
                continue
            assert workspace_manifest is not None
            cargo_roots.setdefault(workspace_manifest, workspace_manifest)
            continue

        try:
            document = json.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            errors.append(f"{manifest.relative_to(root)}: cannot read package.json: {error}")
            continue
        if not isinstance(document, dict):
            errors.append(f"{manifest.relative_to(root)}: package.json must contain an object")
            continue
        for table in NPM_DEPENDENCY_TABLES:
            dependencies = document.get(table, {})
            if not isinstance(dependencies, dict):
                continue
            for dependency, specification in dependencies.items():
                if isinstance(specification, str):
                    for prefix in ("file:", "link:"):
                        if specification.startswith(prefix):
                            check_reference(
                                manifest,
                                f"[{table}]",
                                str(dependency),
                                specification[len(prefix) :],
                            )
                            break
        workspaces = document.get("workspaces", [])
        if isinstance(workspaces, dict):
            workspaces = workspaces.get("packages", [])
        check_workspace_globs(manifest, workspaces)

    checked_workspaces = 0
    checked_package_manifests: set[Path] = set()
    for workspace_manifest, discovered_from in sorted(cargo_roots.items()):
        metadata, error = cargo_metadata(workspace_manifest)
        if error is not None:
            relative = workspace_manifest.relative_to(root).as_posix()
            message = f"{relative}: cargo metadata --locked --offline failed: {error}"
            if relative.startswith("spikes/"):
                unchecked.append(message)
            else:
                errors.append(message)
            continue
        assert metadata is not None
        checked_workspaces += 1
        packages = metadata["packages"]
        # Metadata contains patched packages only when they participate in the
        # resolved graph; an unused [patch] entry cannot add code to the build.
        for package in packages:
            if not isinstance(package, dict) or package.get("source") is not None:
                continue
            try:
                manifest_path = Path(package["manifest_path"]).resolve(strict=False)
            except (KeyError, OSError, RuntimeError, TypeError) as error:
                errors.append(f"{discovered_from.relative_to(root)}: invalid package manifest path: {error}")
                continue
            if manifest_path in checked_package_manifests:
                continue
            checked_package_manifests.add(manifest_path)
            checked_references += 1
            if not inside_repository(manifest_path, root):
                package_name = str(package.get("name", "<unnamed>"))
                violations.append(
                    f"Cargo workspace {discovered_from.relative_to(root)}: package={package_name} "
                    f"manifest={manifest_path} resolved={manifest_path}"
                )

    return violations, unchecked, errors, len(manifests), checked_references, checked_workspaces


def _write(root: Path, relative: str, content: str) -> None:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _cargo_package(directory: Path, name: str, version: str = "0.1.0") -> None:
    _write(
        directory,
        "Cargo.toml",
        f'[package]\nname = "{name}"\nversion = "{version}"\nedition = "2021"\n',
    )
    _write(directory, "src/lib.rs", "pub fn fixture() {}\n")


def _prepare_lock(root: Path) -> str | None:
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--offline",
        "--manifest-path",
        str(root / "Cargo.toml"),
    ]
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, check=False, env=cargo_env()
        )
    except OSError as error:
        return str(error)
    if result.returncode:
        return short_error(result)
    if not (root / "Cargo.lock").exists():
        return "cargo metadata did not create a fixture lockfile"
    return None


def self_test(selected_case: str | None = None) -> int:
    failures = 0
    cases_run = 0

    def verify(
        name: str,
        setup: Any,
        expected_violations: tuple[tuple[str, ...], ...] = (),
        expected_errors: tuple[tuple[str, ...], ...] = (),
        expected_unchecked: tuple[tuple[str, ...], ...] = (),
    ) -> None:
        nonlocal failures, cases_run
        if selected_case is not None and name != selected_case:
            return
        cases_run += 1
        with tempfile.TemporaryDirectory(prefix="check-path-deps-") as temporary:
            temporary_root = Path(temporary)
            root = temporary_root / "repo"
            outside = temporary_root / "outside"
            root.mkdir()
            outside.mkdir()
            try:
                setup(root, outside)
            except (AssertionError, OSError) as error:
                failures += 1
                print(f"check-path-deps self-test: FAIL — {name}: fixture setup failed: {error}", file=sys.stderr)
                return
            violations, unchecked, errors, _, _, _ = check_root(root)

            def matches(actual: list[str], expected: tuple[tuple[str, ...], ...]) -> bool:
                return len(actual) == len(expected) and all(
                    any(all(fragment in line for fragment in group) for line in actual)
                    for group in expected
                )

            passed = (
                matches(violations, expected_violations)
                and matches(errors, expected_errors)
                and matches(unchecked, expected_unchecked)
            )
            if passed:
                print(f"check-path-deps self-test: PASS — {name}")
            else:
                failures += 1
                print(
                    f"check-path-deps self-test: FAIL — {name}: expected violations={expected_violations}, "
                    f"errors={expected_errors}, unchecked={expected_unchecked}; got violations={violations}, "
                    f"errors={errors}, unchecked={unchecked}",
                    file=sys.stderr,
                )

    def direct_path(root: Path, outside: Path) -> None:
        _cargo_package(root, "fixture-root")
        _cargo_package(outside, "outside")
        with (root / "Cargo.toml").open("a", encoding="utf-8") as file:
            file.write('\n[dependencies]\nescape = { package = "outside", path = "../outside" }\n')
        error = _prepare_lock(root)
        assert error is None, error

    verify(
        "[dependencies] path outside repository is refused",
        direct_path,
        (("package=outside", "resolved="),),
    )

    def patched_path(root: Path, outside: Path) -> None:
        _cargo_package(root, "fixture-root")
        _cargo_package(outside, "patched", "1.0.0")
        with (root / "Cargo.toml").open("a", encoding="utf-8") as file:
            file.write(
                '\n[dependencies]\npatched = "1.0.0"\n'
                '[patch.crates-io]\npatched = { path = "../outside" }\n'
            )
        error = _prepare_lock(root)
        assert error is None, error

    verify(
        "[patch.crates-io] path outside repository is refused",
        patched_path,
        (("package=patched", "resolved="),),
    )

    verify(
        "package.json file: path outside repository is refused",
        lambda root, outside: _write(
            root, "package.json", '{"dependencies":{"external":"file:../outside"}}\n'
        ),
        (("table=[dependencies]", "dependency=external", "resolved="),),
    )
    verify(
        "package.json link: path outside repository is refused",
        lambda root, outside: _write(
            root, "package.json", '{"devDependencies":{"external":"link:../outside"}}\n'
        ),
        (("table=[devDependencies]", "dependency=external", "resolved="),),
    )

    def symlink_path(root: Path, outside: Path) -> None:
        _cargo_package(root, "fixture-root")
        _cargo_package(outside, "outside")
        (root / "linked-outside").symlink_to(outside, target_is_directory=True)
        with (root / "Cargo.toml").open("a", encoding="utf-8") as file:
            file.write('\n[dependencies]\nescape = { package = "outside", path = "linked-outside" }\n')
        error = _prepare_lock(root)
        assert error is None, error

    verify(
        "symlinked Cargo path dependency escaping repository is refused",
        symlink_path,
        (("package=outside", "resolved="),),
    )

    def cargo_tables(root: Path, outside: Path) -> None:
        _cargo_package(root, "fixture-root")
        for name in ("build-out", "target-out", "workspace-out"):
            _cargo_package(outside / name, name, "1.0.0")
        with (root / "Cargo.toml").open("a", encoding="utf-8") as file:
            file.write(
                '\n[build-dependencies]\nbuild-out = { path = "../outside/build-out" }\n'
                "[target.'cfg(unix)'.dev-dependencies]\n"
                'target-out = { path = "../outside/target-out" }\n'
                '\n[workspace]\nmembers = []\n'
                '[workspace.dependencies]\nworkspace-out = { path = "../outside/workspace-out" }\n'
                '\n[dependencies]\nworkspace-out = { workspace = true }\n'
            )
        error = _prepare_lock(root)
        assert error is None, error

    verify(
        "Cargo build, target, and workspace dependency tables resolve through metadata",
        cargo_tables,
        (("package=build-out",), ("package=target-out",), ("package=workspace-out",)),
    )

    def inside_only(root: Path, outside: Path) -> None:
        _cargo_package(root, "fixture-root")
        _cargo_package(root / "crates/inside", "inside")
        with (root / "Cargo.toml").open("a", encoding="utf-8") as file:
            file.write(
                '\n[workspace]\nmembers = ["crates/inside"]\n'
                '[dependencies]\ninside = { path = "crates/inside" }\n'
            )
        _write(
            root,
            "package.json",
            '{"dependencies":{"file-dep":"file:./vendor/file-dep",'
            '"link-dep":"link:./vendor/link-dep"},"workspaces":["packages/*"]}\n',
        )
        _write(root, "vendor/file-dep/package.json", "{}\n")
        _write(root, "vendor/link-dep/package.json", "{}\n")
        _write(root, "packages/app/package.json", "{}\n")
        _write(root, "target/Cargo.toml", '[dependencies]\nignored = { path = "../../outside" }\n')
        _write(root, "node_modules/ignored/package.json", '{"dependencies":{"bad":"file:../../outside"}}\n')
        _write(root, ".git/ignored/Cargo.toml", '[dependencies]\nignored = { path = "../../outside" }\n')
        error = _prepare_lock(root)
        assert error is None, error

    verify(
        "inside-repository dependencies and workspace members pass; generated trees are skipped",
        inside_only,
    )

    verify(
        "package workspace glob outside repository is refused",
        lambda root, outside: _write(root, "package.json", '{"workspaces":["../../outside/*"]}\n'),
        (("table=[workspaces]", "dependency=../../outside/*", "resolved="),),
    )

    def outside_workspace_member(root: Path, outside: Path) -> None:
        _cargo_package(outside, "outside-member")
        _write(root, "Cargo.toml", '[workspace]\nmembers = ["../outside*"]\n')

    verify(
        "Cargo workspace member glob outside repository is not accepted",
        outside_workspace_member,
        expected_errors=(("not hierarchically below the workspace root",),),
    )

    if selected_case is not None and cases_run == 0:
        print(f"check-path-deps self-test: unknown case {selected_case!r}", file=sys.stderr)
        return 2
    if failures:
        print(f"check-path-deps self-test: {failures} check(s) failed", file=sys.stderr)
        return 1
    print(f"check-path-deps self-test: all {cases_run} checks passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, help="repository root to check (defaults to this script's repository)")
    parser.add_argument("--self-test", action="store_true", help="run fixture-based self-tests")
    parser.add_argument("--self-test-case", help="run one named self-test case for mutation checks")
    args = parser.parse_args()
    if args.self_test or args.self_test_case:
        if args.root is not None or (args.self_test_case and not args.self_test):
            parser.error("self-test options cannot be combined with --root, and --self-test-case requires --self-test")
        return self_test(args.self_test_case)

    root = args.root if args.root is not None else Path(__file__).resolve().parent.parent
    try:
        violations, unchecked, errors, manifest_count, reference_count, workspace_count = check_root(root)
    except (OSError, RuntimeError) as error:
        print(f"check-path-deps: cannot inspect repository {root}: {error}", file=sys.stderr)
        return 2
    for violation in violations:
        print(f"check-path-deps: OUTSIDE {violation}", file=sys.stderr)
    for error in errors:
        print(f"check-path-deps: ERROR {error}", file=sys.stderr)
    for item in unchecked:
        print(f"check-path-deps: UNCHECKED {item}", file=sys.stderr)
    if violations or errors:
        return 1

    summary = (
        f"checked {manifest_count} manifests and {workspace_count} Cargo workspaces, "
        f"{reference_count} resolved path references inside the repository"
    )
    if unchecked:
        summary += f"; unchecked Cargo manifests: {', '.join(item.split(':', 1)[0] for item in unchecked)}"
    print(summary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
