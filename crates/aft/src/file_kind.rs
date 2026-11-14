pub use aft_base::file_kind::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::Read;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Component, Path};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::SystemTime;

#[cfg(test)]
mod tests {
    #[test]
    fn extension_only_source_classification_matches_parser() {
        use crate::lang_id::{LangId, TEST_EXTENSIONS};
        for name in TEST_EXTENSIONS
            .iter()
            .map(|ext| format!("file.{ext}"))
            .chain(["Jenkinsfile", "file", ".hidden"].map(str::to_owned))
        {
            let expected = matches!(crate::parser::detect_language(std::path::Path::new(&name)),
                Some(lang) if !matches!(lang, LangId::Markdown | LangId::Html | LangId::Yaml | LangId::Toml));
            assert_eq!(super::is_source_file_name(&name), expected, "{name}");
        }
    }

    use super::*;

    fn kind_of(root: &Path, relative: &str) -> FileKind {
        classify(root, &root.join(relative), None)
    }

    fn content_of(relative: &str) -> (ContentKind, Option<&'static str>) {
        let root = Path::new("/project");
        let kind = classify(root, &root.join(relative), Some(b"".as_slice()));
        (kind.content, kind.marker)
    }

    #[test]
    fn file_kind_every_lockfile_name_is_a_lockfile_marked_by_its_name() {
        for name in LOCKFILE_NAMES {
            assert_eq!(
                content_of(&format!("sub/{name}")),
                (ContentKind::Lockfile, Some(*name)),
                "{name}"
            );
            let lower = name.to_ascii_lowercase();
            assert_eq!(
                content_of(&lower).0,
                ContentKind::Lockfile,
                "lowercased {lower}"
            );
        }
        assert_eq!(
            content_of("vendor/custom.lock"),
            (ContentKind::Lockfile, Some(".lock"))
        );
    }

    #[test]
    fn file_kind_capture_names_are_logs_or_command_output() {
        let cases: &[(&str, &str)] = &[
            ("runs/io/stdout", "stdout"),
            ("runs/io/stderr", "stderr"),
            ("runs/stderr.txt", "stderr"),
            ("receipts/bash-1.stdout.txt", "stdout"),
            ("receipts/run.stdout.out", "stdout"),
            ("receipts/run.stderr.log", ".log"),
            ("var/app.log", ".log"),
            ("var/app.log.1", ".log"),
            ("var/app.log.20", ".log"),
            ("records/x.jsonl", ".jsonl"),
            ("records/x.ndjson", ".ndjson"),
            ("receipts/build.stdout", ".stdout"),
            ("receipts/build.stderr", ".stderr"),
        ];
        for (path, marker) in cases {
            assert_eq!(
                content_of(path),
                (ContentKind::Capture, Some(*marker)),
                "{path}"
            );
        }
    }

    #[test]
    fn file_kind_capture_rule_never_catches_source_docs_or_folder_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let never = [
            "src/log.rs",
            "src/logger.ts",
            "src/stdout.rs",
            "src/stderr.ts",
            "tools/capture.py",
            "tools/capture-sink.js",
            "src/foo.log.ts",
            "CHANGELOG.md",
            "changelog",
            "notes/release.log.md",
            "test/common-auth-094-adoption/log/contract.mjs",
            "logs/README.md",
            "receipts/README.md",
            "research/v1/analysis.py",
            "tests/golden/expected.out",
            "tests/golden/expected.err",
            "src/stdout_writer.go",
        ];
        for path in never {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "const needle = 1;\n").unwrap();
            assert_eq!(kind_of(root, path).content, ContentKind::Text, "{path}");
        }
    }

    #[test]
    fn file_kind_minified_and_generated_names() {
        assert_eq!(
            content_of("vendor/highlight.min.js"),
            (ContentKind::Minified, Some(".min.js"))
        );
        assert_eq!(
            content_of("dist/site.MIN.CSS"),
            (ContentKind::Minified, Some(".min.css"))
        );
        assert_eq!(
            content_of("dist/app.js.map"),
            (ContentKind::Generated, Some(".js.map"))
        );
        assert_eq!(
            content_of("src/generated/types.ts"),
            (ContentKind::Generated, None)
        );
        assert_eq!(content_of("src/api_pb.ts"), (ContentKind::Generated, None));
        // A bare `.map` is too generic to mean a source map.
        assert_eq!(content_of("data/world.map").0, ContentKind::Text);
        // Folders above the project root never decide a generated shape.
        let kind = classify(
            Path::new("/home/gen/project"),
            Path::new("/home/gen/project/src/main.rs"),
            Some(b"fn main() {}\n".as_slice()),
        );
        assert_eq!(kind.content, ContentKind::Text);
    }

    #[test]
    fn file_kind_path_outside_the_root_is_decided_by_its_file_name() {
        // A file the root does not contain: its folders (`gen/`) never
        // decide, but a generated file name still does.
        let root = Path::new("/project");
        let text = b"fn main() {}\n".as_slice();
        let kind = classify(root, Path::new("/elsewhere/gen/main.rs"), Some(text));
        assert_eq!(kind.content, ContentKind::Text);
        let kind = classify(root, Path::new("/elsewhere/gen/api_pb.ts"), Some(text));
        assert_eq!(kind.content, ContentKind::Generated);
    }

    #[cfg(windows)]
    #[test]
    fn file_kind_windows_folders_above_the_root_never_decide() {
        let text = b"fn main() {}\n".as_slice();
        let content =
            |root: &str, path: &str| classify(Path::new(root), Path::new(path), Some(text)).content;
        // Drive-letter root and file.
        let root = r"C:\Users\gen\project";
        assert_eq!(
            content(root, r"C:\Users\gen\project\src\main.rs"),
            ContentKind::Text
        );
        assert_eq!(
            content(root, r"C:\Users\gen\project\src\generated\types.ts"),
            ContentKind::Generated
        );
        // A `\\?\`-prefixed root (the form Windows canonicalization returns)
        // with a plainly spelled file.
        let verbatim = r"\\?\C:\Users\gen\project";
        assert_eq!(
            content(verbatim, r"C:\Users\gen\project\src\main.rs"),
            ContentKind::Text
        );
        assert_eq!(
            content(verbatim, r"C:\Users\gen\project\gen\schema.ts"),
            ContentKind::Generated
        );
        // A root with no drive letter, which `Path::is_absolute` calls
        // relative on Windows.
        assert_eq!(
            content(r"\home\gen\project", r"\home\gen\project\src\main.rs"),
            ContentKind::Text
        );
        // A file whose spelling cannot be matched to the root (no drive
        // letter, or a Windows short-name folder alias such as `PROJEC~1`): only
        // its file name decides.
        assert_eq!(
            content(root, r"\Users\gen\project\src\main.rs"),
            ContentKind::Text
        );
        assert_eq!(
            content(root, r"C:\Users\gen\PROJEC~1\src\main.rs"),
            ContentKind::Text
        );
        assert_eq!(
            content(root, r"C:\Users\gen\PROJEC~1\src\api_pb.ts"),
            ContentKind::Generated
        );
    }

    #[test]
    fn file_kind_content_signals_decide_only_when_the_name_does_not() {
        let root = Path::new("/project");
        let header = b"// Code generated by protoc-gen-es. DO NOT EDIT.\nexport const x = 1;\n";
        let kind = classify(root, &root.join("src/api.ts"), Some(header.as_slice()));
        assert_eq!((kind.content, kind.marker), (ContentKind::Generated, None));

        let bundle = "x".repeat(4 * 1024);
        let kind = classify(root, &root.join("dist/bundle.js"), Some(bundle.as_bytes()));
        assert_eq!((kind.content, kind.marker), (ContentKind::Minified, None));

        // Docs are never read for headers: their text is what a reader wants.
        let kind = classify(root, &root.join("docs/api.md"), Some(header.as_slice()));
        assert_eq!(kind.content, ContentKind::Text);

        let source: String = (0..200).map(|i| format!("const v{i} = {i};\n")).collect();
        let kind = classify(root, &root.join("src/app.js"), Some(source.as_bytes()));
        assert_eq!(kind.content, ContentKind::Text);
    }

    #[test]
    fn file_kind_json_data_needs_a_long_line_and_exempts_schemas_and_manifests() {
        let root = Path::new("/project");
        let long_line = format!("{{\"blob\": \"{}\"}}\n", "a".repeat(300));
        let classify_json = |name: &str, body: &str| {
            let kind = classify(root, &root.join(name), Some(body.as_bytes()));
            (kind.content, kind.marker)
        };
        assert_eq!(
            classify_json("research/mutations.json", &long_line),
            (ContentKind::DataJson, Some(".json"))
        );
        assert_eq!(
            classify_json("maps/world.geojson", &long_line),
            (ContentKind::DataJson, Some(".geojson"))
        );
        assert_eq!(
            classify_json("assets/aft.schema.json", &long_line).0,
            ContentKind::Text
        );
        let with_schema_key = format!("{{\"$schema\": \"x\",\n{long_line}");
        assert_eq!(
            classify_json("config/tool.json", &with_schema_key).0,
            ContentKind::Text
        );
        assert_eq!(
            classify_json("package.json", &long_line).0,
            ContentKind::Text
        );
        assert_eq!(
            classify_json("tsconfig.build.json", &long_line).0,
            ContentKind::Text
        );
        assert_eq!(
            classify_json(".eslintrc.json", &long_line).0,
            ContentKind::Text
        );
        // Size alone does not make data: 47 KB of short lines stays text.
        let short_lines: String = (0..700)
            .map(|i| format!("  \"entry{i}\": \"value of about one hundred bytes ............................\",\n"))
            .collect();
        assert!(short_lines.len() > 40 * 1024);
        assert_eq!(
            classify_json("src/source-output.json", &short_lines).0,
            ContentKind::Text
        );
    }

    #[test]
    fn file_kind_json_long_line_test_reads_at_most_64_kib() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let short_lines = "  \"k\": \"v\",\n".repeat(JSON_HEAD_BYTES / 12 + 100);
        assert!(short_lines.len() > JSON_HEAD_BYTES);
        let late = root.join("late.json");
        std::fs::write(
            &late,
            format!("{short_lines}\"blob\": \"{}\"\n", "a".repeat(400)),
        )
        .unwrap();
        assert_eq!(kind_of(root, "late.json").content, ContentKind::Text);
        assert!(read_probe::bytes_read(&late) <= JSON_HEAD_BYTES);

        let early = root.join("early.json");
        std::fs::write(
            &early,
            format!("\"blob\": \"{}\"\n{short_lines}", "a".repeat(400)),
        )
        .unwrap();
        assert_eq!(kind_of(root, "early.json").content, ContentKind::DataJson);
        assert!(read_probe::bytes_read(&early) <= JSON_HEAD_BYTES);
    }

    /// The index store's minified check agrees with the file kinds: by
    /// name, or a JavaScript or CSS head with almost no line breaks, and
    /// never a source file of another language however dense.
    #[test]
    fn is_minified_matches_the_minified_file_kind() {
        let dense = format!("var a=1;{}", "b();".repeat(2048));
        let lines = "export function a() {}\n".repeat(400);
        assert!(is_minified("web/app.min.js", lines.as_bytes()));
        assert!(is_minified("web/STYLE.MIN.CSS", b""));
        assert!(is_minified("web/bundle.js", dense.as_bytes()));
        assert!(!is_minified("web/app.js", lines.as_bytes()));
        assert!(!is_minified("src/lib.rs", dense.as_bytes()));
        assert!(!is_minified("web.min.js/app.ts", lines.as_bytes()));
        for (path, bytes) in [
            ("bundle.js", dense.as_bytes()),
            ("app.js", lines.as_bytes()),
        ] {
            let kind = classify(Path::new("/p"), &Path::new("/p").join(path), Some(bytes));
            assert_eq!(
                kind.content == ContentKind::Minified,
                is_minified(path, bytes),
                "{path}"
            );
        }
    }

    #[test]
    fn file_kind_single_line_bundle_is_read_at_most_8_kib() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bundle = root.join("bundle.js");
        std::fs::write(&bundle, "x".repeat(10 * 1024 * 1024)).unwrap();

        // inspect's generated-header check used to read the whole first line.
        assert!(!generated::is_generated_file(root, &bundle));
        let after_inspect = read_probe::bytes_read(&bundle);
        assert!(
            after_inspect <= HEAD_BYTES,
            "inspect read {after_inspect} bytes of a single-line bundle"
        );

        assert_eq!(kind_of(root, "bundle.js").content, ContentKind::Minified);
        let after_classify = read_probe::bytes_read(&bundle) - after_inspect;
        assert!(
            after_classify <= HEAD_BYTES,
            "classify read {after_classify} bytes of a single-line bundle"
        );
    }

    #[test]
    fn file_kind_held_bytes_and_cache_avoid_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("app.ts");
        std::fs::write(&file, "export const x = 1;\n").unwrap();
        classify(root, &file, Some(b"export const x = 1;\n".as_slice()));
        assert_eq!(read_probe::bytes_read(&file), 0);

        let mut cache = FileKindCache::new();
        cache.classify(root, &file, None);
        let first = read_probe::bytes_read(&file);
        assert!(first > 0);
        cache.classify(root, &file, None);
        assert_eq!(read_probe::bytes_read(&file), first);
    }

    #[test]
    fn file_kind_test_role_is_independent_of_content() {
        let kind = classify(
            Path::new("/project"),
            Path::new("/project/tests/fixtures/run/stderr"),
            None,
        );
        assert_eq!(kind.content, ContentKind::Capture);
        assert_eq!(kind.test, TestRole::Test);
        let kind = classify(
            Path::new("/project"),
            Path::new("/project/src/__fixtures__/run/stderr"),
            None,
        );
        assert_eq!(
            (kind.content, kind.test),
            (ContentKind::Capture, TestRole::Support)
        );
        let kind = classify(
            Path::new("/project"),
            Path::new("/project/src/app.test.ts"),
            Some(b"".as_slice()),
        );
        assert_eq!(
            (kind.content, kind.test),
            (ContentKind::Text, TestRole::Test)
        );
    }
}

#[cfg(test)]
mod test_support_tests {
    use super::{is_test_file, is_test_support_file};

    #[test]
    fn is_test_file_matches_real_test_files() {
        // JS/TS family: *.test.* / *.spec.*
        for p in [
            "src/foo.test.ts",
            "src/foo.test.tsx",
            "src/bar.spec.js",
            "packages/x/component.test.jsx",
            "app/foo.test.mjs",
            "src/comp.spec.vue",
        ] {
            assert!(is_test_file(p), "{p} should be a test file");
        }
        // __tests__ directory and tests roots.
        assert!(is_test_file("packages/x/__tests__/reading.ts"));
        assert!(is_test_file("crates/aft/tests/integration/main.rs"));
        // Per-language filename suffixes.
        assert!(is_test_file("crates/aft/src/foo_test.rs"));
        assert!(is_test_file("pkg/handler_test.go"));
        assert!(is_test_file("app/test_models.py"));
        assert!(is_test_file("app/models_test.py"));
        assert!(is_test_file("spec/user_spec.rb"));
        // CamelCase test classes (case-sensitive).
        assert!(is_test_file("src/main/UserServiceTest.java"));
        assert!(is_test_file("src/FooTests.cs"));
        assert!(is_test_file("Sources/AppTests.swift"));
        // Windows separators normalize.
        assert!(is_test_file("packages\\x\\__tests__\\a.ts"));
    }

    #[test]
    fn is_test_file_rejects_product_files() {
        for p in [
            "crates/aft/src/inspect/job.rs",
            "packages/x/src/index.ts",
            "src/contestant.ts",     // "test" substring, not a test file
            "src/greatest.ts",       // ends with "test" stem, not ".test."
            "src/latest.java",       // case-sensitive guard: not "Test.java"
            "src/my_attestation.py", // not test_*/*_test
            "src/test/helper.ts",    // singular `test` dir must not blanket-match
        ] {
            assert!(!is_test_file(p), "{p} must NOT be a test file");
        }
    }

    #[test]
    fn matches_conventional_support_dirs() {
        assert!(is_test_support_file("crates/aft/tests/fixtures/sample.ts"));
        assert!(is_test_support_file(
            "packages/x/__tests__/e2e/fixtures/a.ts"
        ));
        assert!(is_test_support_file(
            "benchmarks/codegraph/corpora/repo/lib.go"
        ));
        assert!(is_test_support_file("src/__mocks__/fs.ts"));
        assert!(is_test_support_file("src/__snapshots__/render.snap"));
        assert!(is_test_support_file("internal/testdata/golden.json"));
        // Windows-style separators normalize.
        assert!(is_test_support_file("crates\\aft\\tests\\fixtures\\x.rs"));
    }

    #[test]
    fn does_not_match_product_or_test_files() {
        // A real product file under src must never be excluded.
        assert!(!is_test_support_file("crates/aft/src/inspect/job.rs"));
        // Test FILES are not support files (they hold real assertions/roots).
        assert!(!is_test_support_file(
            "packages/x/__tests__/reading.test.ts"
        ));
        assert!(!is_test_support_file(
            "crates/aft/tests/integration/main.rs"
        ));
        // Substring of a segment must not match (only whole segments).
        assert!(!is_test_support_file("src/fixturesHelper.ts"));
        assert!(!is_test_support_file("src/my_corpora_loader.rs"));
    }
}

#[cfg(test)]
mod generated;
