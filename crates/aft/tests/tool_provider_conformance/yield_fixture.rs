use super::*;

const BASH_DESCRIPTION: &str = "Wait for the yield fixture release";

pub(super) type BashReady = Arc<Mutex<HashMap<String, PathBuf>>>;

pub(super) fn bash_ready_dir(root: &Path) -> PathBuf {
    root.join("bash-wait-ready")
}

pub(super) fn configure(command: &mut Command, root: &Path) -> Result<(), HarnessError> {
    let dir = bash_ready_dir(root);
    std::fs::create_dir_all(&dir).map_err(harness_error)?;
    command.env("AFT_TEST_CONFORMANCE_BASH_WAIT_READY", dir);
    Ok(())
}

/// Match the conformance call key to the exact request on this provider connection.
pub(super) fn track_bash_call(route: &Route, body: &Value, request: &Frame) {
    if body["name"] != "bash" || body["arguments"]["description"] != BASH_DESCRIPTION {
        return;
    }
    let (Some((dir, ready)), Some(key)) = (&route.yield_bash_ready, body["call_key"].as_str())
    else {
        return;
    };
    let header = &request.header;
    let marker = dir.join(format!(
        "{}-{}-{}",
        header.channel, header.epoch, header.corr
    ));
    ready.lock().unwrap().insert(key.to_owned(), marker);
}

pub(super) struct ReleaseGuard<'a>(pub(super) &'a Subject);

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        let releases: Vec<_> = self.0.yield_releases.lock().unwrap().drain(..).collect();
        for path in &releases {
            let _ = std::fs::write(path, b"release");
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while releases
            .iter()
            .any(|path| !path.with_extension("done").exists())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub(super) fn call(subject: &Subject, tagged: bool) -> Option<CallSpec> {
    // A call returned while it is still running needs a session for its
    // completion notice. Worker calls have no session to wake, and reader
    // routes do not offer a shell tool.
    if subject.preset == Some("worker") || !subject.serves_shell() || cfg!(windows) {
        return None;
    }
    let project = subject.project.lock().unwrap().clone()?;
    let sequence = subject.yield_sequence.fetch_add(1, Ordering::SeqCst);
    let file = project.join(format!("yield-probe-{sequence}.txt"));
    *subject.yield_probe.lock().unwrap() = Some((file.clone(), tagged));
    if tagged {
        let release = file.with_extension("release");
        let done = release.with_extension("done");
        let release = release.display().to_string().replace('\'', "'\\''");
        let command = format!(
            "while [ ! -e '{}' ]; do sleep 0.2; done; {}",
            release,
            marker_command(&done)
        );
        Some((
            "bash".into(),
            json!({"command": command, "description": BASH_DESCRIPTION, "wait": true, "timeout": 300000}),
        ))
    } else {
        std::fs::write(&file, b"read fixture\n").expect("write read fixture");
        Some(("read".into(), json!({"path": file})))
    }
}

pub(super) async fn wait(subject: &Subject, key: &str) -> Result<(), HarnessError> {
    let (file, tagged) = subject
        .yield_probe
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| HarnessError::new("no call-yield probe"))?;
    let release = file.with_extension("release");
    if tagged {
        subject.yield_releases.lock().unwrap().push(release.clone());
    }
    let mut ready = (!tagged).then(|| file.with_extension("held"));
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if ready.is_none() {
                ready = subject.yield_bash_ready.lock().unwrap().remove(key);
            }
            // Only the provider knows when a running shell can be yielded.
            if ready.as_ref().is_some_and(|path| path.is_file()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| {
        HarnessError::new(format!(
            "call-yield probe {key:?} did not become ready: {ready:?}"
        ))
    })?;
    if !tagged {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = std::fs::write(release, b"release");
        });
    }
    Ok(())
}
