use super::*;

fn manifest() -> Manifest {
    let mut manifest = v22_tests::manifest();
    let additions: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v23-additions.json"
    ))
    .unwrap();
    manifest.manifest_version = 23;
    manifest.tiers.get_mut(&Tier::Governed).unwrap().extend(
        serde_json::from_value::<Vec<TupleDecl>>(additions["tiers"]["governed"].clone()).unwrap(),
    );
    manifest.canonicalization.extend(
        serde_json::from_value::<BTreeMap<String, Canonicalization>>(
            additions["canonicalization"].clone(),
        )
        .unwrap(),
    );
    manifest.validate().unwrap();
    manifest
}

#[test]
fn queue_rows_require_both_v23_and_the_signed_declaration_on_every_platform() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    for (argv, expected) in [
        (
            vec!["pr", "merge", "7", "--match-head-commit", sha],
            "pr enqueue",
        ),
        (
            vec!["pr", "merge", "7", "--auto", "--match-head-commit", sha],
            "pr enqueue",
        ),
        (vec!["pr", "dequeue", "7"], "pr dequeue"),
    ] {
        let args = argv.iter().map(OsString::from).collect::<Vec<_>>();
        for platform in ["macos", "linux", "windows"] {
            let mut current = manifest();
            assert!(
                matches!(classify(&args, &current, platform), Classification::Governed { tuple, .. } if tuple == expected)
            );
            current.manifest_version = 22;
            assert!(!matches!(
                classify(&args, &current, platform),
                Classification::Governed { .. }
            ));
            current.manifest_version = 23;
            current
                .tiers
                .get_mut(&Tier::Governed)
                .unwrap()
                .retain(|row| row.tuple() != expected);
            current.canonicalization.remove(expected);
            assert!(!matches!(
                classify(&args, &current, platform),
                Classification::Governed { .. }
            ));
        }
    }
}

#[test]
fn direct_merge_flags_and_disable_auto_keep_their_existing_classification() {
    let current = manifest();
    let previous = v22_tests::manifest();
    for flag in [
        "--merge",
        "-m",
        "--squash",
        "-s",
        "--rebase",
        "-r",
        "--admin",
        "--disable-auto",
        "-dm",
    ] {
        let args = [
            "pr",
            "merge",
            "7",
            flag,
            "--match-head-commit",
            "0123456789abcdef0123456789abcdef01234567",
        ]
        .map(OsString::from);
        for platform in ["macos", "linux", "windows"] {
            assert_eq!(
                format!("{:?}", classify(&args, &current, platform)),
                format!("{:?}", classify(&args, &previous, platform)),
                "{flag} on {platform}"
            );
        }
    }
}

#[test]
fn dequeue_body_free_wire_retains_captured_mutation_semantics() {
    let current = manifest();
    let args = ["pr", "dequeue", "7", "--repo", "cortexkit/aft"].map(OsString::from);
    let canonical = &current.canonicalization["pr dequeue"];
    let request =
        canonicalize_governed_from(&args, "pr dequeue", canonical, 23, &mut std::io::empty())
            .unwrap();
    let semantics = mutation_semantics(&request);
    let mutation = GithubReadMutation::from_governed_request(&request).unwrap();
    assert_eq!(mutation.resource_kind, GithubReadResourceKind::PullRequest);
    assert_eq!(mutation.resource_number, 7);
    let wire = governed_wire_request(
        &RungDetermination::r1(1, R1Reason::AbsentOrUnparseable).record,
        "alfonso-aft",
        request,
    );
    assert!(wire.get("body").is_none());
    assert_eq!(wire_mutation_semantics(&wire), Some(semantics));
    assert_eq!(plexus_contract_judge::judge(&wire), Ok("pr_dequeue"));
}

#[test]
fn v23_preparer_preserves_every_baseline_field_except_the_declared_additions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir().unwrap();
    let script = root.join("scripts/prepare-gh-shim-v23.py");
    // The production payload is not tracked. Exercise the preparer with a
    // synthetic baseline and a test-only replacement digest.
    let probe = r#"import hashlib, importlib.util, json, pathlib, sys
spec = importlib.util.spec_from_file_location('prepare', sys.argv[1])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
baseline = json.loads(sys.argv[3])
baseline['manifest_version'] = 22
raw = m.encode(baseline).encode('utf-8')
p = pathlib.Path(sys.argv[2])
src, dst = p / 'v22.json', p / 'v23.json'
src.write_bytes(raw)
try:
    m.prepare(src, dst, 1800000000)
except ValueError as err:
    assert 'expected reviewed v22 baseline' in str(err)
else:
    raise AssertionError('unreviewed baseline accepted')
assert not dst.exists()
m.BASELINE_SHA256 = hashlib.sha256(raw).hexdigest()
payload = m.prepare(src, dst, 1800000000)
current = json.loads(payload)
assert current['manifest_version'] == 23
assert current['issued_at_unix_secs'] == 1800000000
added = current['tiers']['governed'][-2:]
assert [row['tuple'] for row in added] == ['pr enqueue', 'pr dequeue']
assert all(row['platform'] == ['macos', 'linux', 'windows'] for row in added)
current['tiers']['governed'] = current['tiers']['governed'][:-2]
for name in ['pr enqueue', 'pr dequeue']:
    del current['canonicalization'][name]
current['manifest_version'] = baseline['manifest_version']
current['issued_at_unix_secs'] = baseline['issued_at_unix_secs']
assert m.encode(current).encode('utf-8') == raw
assert dst.with_suffix('.sha256').read_text() == hashlib.sha256(payload).hexdigest() + '  v23.json\n'
print('v23 preparer: 8 invariants passed')
"#;
    let baseline: Value = serde_json::to_value(v22_tests::manifest()).unwrap();
    let output = Command::new("python3")
        .args(["-c", probe])
        .arg(script)
        .arg(dir.path())
        .arg(baseline.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("8 invariants passed"));
}
