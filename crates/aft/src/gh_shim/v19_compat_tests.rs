use super::*;

include!("../../tests/fixtures/gh_shim/status_normalization.rs");

const MAIN: &str = "bdbf17caf5d0dae171399937876d454e0974b55a";

#[test]
fn audit_v19_classifier_matches_pinned_main_corpus() {
    let _lock = crate::test_env::process_env_lock();
    let input = include_str!("../../tests/fixtures/gh_shim/v19-main-classifier-corpus.json");
    let corpus: Value = serde_json::from_str(input).unwrap();
    let oracle: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v19-main-classifier-decisions.json"
    ))
    .unwrap();
    assert_eq!(corpus["reference_commit"], MAIN);
    assert_eq!(oracle["reference_commit"], MAIN);
    assert_eq!(
        format!("{:x}", Sha256::digest(input.as_bytes())),
        oracle["corpus_sha256"]
    );
    let manifest_bytes = include_str!("../../tests/fixtures/gh_shim/v19-manifest.json");
    assert_eq!(
        format!("{:x}", Sha256::digest(manifest_bytes.as_bytes())),
        oracle["manifest_sha256"]
    );
    let manifest: Manifest = serde_json::from_str(manifest_bytes).unwrap();
    let commands = corpus["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 164);
    assert_eq!(corpus["platforms"], json!(["linux", "macos", "windows"]));
    let expected = oracle["decisions"].as_array().unwrap();
    assert_eq!(expected.len(), 492);
    if let Some(probe) = std::env::var_os("AFT_GH_MAIN_CLASSIFIER_PROBE") {
        let binary = std::env::var_os("AFT_GH_MAIN_BINARY")
            .expect("live main probe requires its production binary");
        let corpus_path = Path::new(crate::test_support::manifest_dir())
            .join("tests/fixtures/gh_shim/v19-main-classifier-corpus.json");
        let output = Command::new(probe)
            .args([
                "--exact",
                "gh_shim::reference_probe::capture_v19",
                "--ignored",
                "--nocapture",
            ])
            .env("AFT_REFERENCE_CORPUS", corpus_path)
            .env("AFT_REFERENCE_BINARY", binary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("1 passed"));
        let live: Vec<Value> = stdout
            .lines()
            .filter_map(|line| line.strip_prefix("REFERENCE_CASE "))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            &live, expected,
            "live pinned main must agree with the frozen oracle"
        );
        println!(
            "Executed pinned main classifier for {} live decisions",
            live.len()
        );
    }
    let mut compared = 0;
    let mut coverage = BTreeSet::new();
    for record in expected {
        let argv: Vec<OsString> = commands[record["index"].as_u64().unwrap() as usize]["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| OsString::from(value.as_str().unwrap()))
            .collect();
        let platform = record["platform"].as_str().unwrap();
        assert!(["linux", "macos", "windows"].contains(&platform));
        assert!(coverage.insert((record["index"].as_u64().unwrap(), platform)));
        let actual = match classify(&argv, &manifest, platform) {
            Classification::Mechanical => json!({"class":"mechanical"}),
            Classification::Admin { tuple } => json!({"class":"admin","tuple":tuple}),
            Classification::Governed { tuple, canonical } => {
                json!({"class":"governed","tuple":tuple,"canonical":canonical})
            }
            Classification::Unclassified => json!({"class":"unclassified"}),
            Classification::Destructive => json!({"class":"destructive"}),
        };
        assert_eq!(
            actual, record["decision"],
            "main vs branch: {platform} {argv:?}"
        );
        compared += 1;
    }
    assert_eq!(compared, 492);
    println!("Compared 492 decisions against main {MAIN}");
}
