use aft::protocol::RawRequest;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// Hold one explicitly named read before dispatch, so conformance tests can
/// check the refusal to yield a waiting read. The read itself is unchanged.
pub(crate) fn wait(req: &RawRequest) {
    if req.command != "read" {
        return;
    }
    let Some(dir) = std::env::var_os("AFT_TEST_CONFORMANCE_READ_GATE").map(PathBuf::from) else {
        return;
    };
    let Some(file) = req
        .params
        .get("file")
        .or_else(|| req.params.get("path"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
    else {
        return;
    };
    if file.parent() != Some(dir.as_path())
        || !file
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("yield-probe-"))
    {
        return;
    }
    let held = file.with_extension("held");
    let release = file.with_extension("release");
    if std::fs::write(held, b"held").is_err() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}
