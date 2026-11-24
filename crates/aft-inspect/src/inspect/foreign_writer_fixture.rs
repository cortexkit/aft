use super::*;

pub struct ForeignWriter {
    child: std::process::Child,
    _stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl Drop for ForeignWriter {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let status = self.child.wait().expect("join foreign inspect writer");
        if !std::thread::panicking() {
            assert!(status.success(), "foreign writer failed: {status}");
        }
    }
}

pub fn foreign_writer(
    root: &Path,
    inspect_dir: &Path,
    snapshot: bool,
    selector: &str,
) -> ForeignWriter {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", selector, "--nocapture"])
        .env("AFT_INSPECT_TEST_WRITER_ROOT", root)
        .env("AFT_INSPECT_TEST_WRITER_DIR", inspect_dir)
        .env(
            "AFT_INSPECT_TEST_WRITER_SNAPSHOT",
            if snapshot { "1" } else { "0" },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn foreign writer");
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "writer exited before ready"
        );
        if line.trim() == "inspect-writer-ready" {
            break;
        }
    }
    ForeignWriter {
        child,
        _stdout: output,
    }
}

pub fn foreign_writer_process() {
    use std::io::{Read, Write};
    let Some(root) = std::env::var_os("AFT_INSPECT_TEST_WRITER_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let inspect_dir = PathBuf::from(std::env::var_os("AFT_INSPECT_TEST_WRITER_DIR").unwrap());
    crate::root_cache::configure_artifact_access(&root, "shared", false);
    let key = crate::path_identity::project_scope_key(&root);
    let dir = project_inspect_dir(inspect_dir.clone(), &key);
    fs::create_dir_all(&dir).unwrap();
    let (cache, lease) = if std::env::var("AFT_INSPECT_TEST_WRITER_SNAPSHOT").unwrap() == "1" {
        let cache = InspectCache::open(inspect_dir, root).unwrap();
        cache
            .store_tier2_result(
                JobKey::for_project_category(InspectCategory::Duplicates),
                &[],
                &[],
                serde_json::json!({"count": 17}),
            )
            .unwrap();
        (Some(cache), None)
    } else {
        let lease = crate::root_cache::WriterLease::acquire_shared_until(
            crate::root_cache::RootCacheDomain::Inspect,
            &dir,
            &key,
            &root,
            crate::fs_lock::acquisition_now() + Duration::from_secs(2),
        )
        .unwrap()
        .expect("writer admitted");
        (None, Some(lease))
    };
    println!("inspect-writer-ready");
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read(&mut [0]);
    let lease = cache
        .as_ref()
        .and_then(|cache| cache.writer_lease.as_ref())
        .or(lease.as_ref())
        .unwrap();
    assert!(
        lease.verify().unwrap(),
        "the foreign writer must keep its lease"
    );
}
