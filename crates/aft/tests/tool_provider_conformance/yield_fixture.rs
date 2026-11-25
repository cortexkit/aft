use super::*;

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
    // A handed-back call needs a session for its completion notice; worker
    // calls have no session to wake, and the reader has no shell tool.
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
            "{}; while [ ! -e '{}' ]; do sleep 0.2; done; {}",
            marker_command(&file),
            release,
            marker_command(&done)
        );
        Some((
            "bash".into(),
            json!({"command": command, "wait": true, "timeout": 300000}),
        ))
    } else {
        std::fs::write(&file, b"read fixture\n").expect("write read fixture");
        Some(("read".into(), json!({"path": file})))
    }
}

pub(super) async fn wait(subject: &Subject, _key: &str) -> Result<(), HarnessError> {
    let (file, tagged) = subject
        .yield_probe
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| HarnessError::new("no call-yield probe"))?;
    let ready = if tagged {
        file.clone()
    } else {
        file.with_extension("held")
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err(HarnessError::new(format!(
                "call-yield probe did not start: {}",
                ready.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if tagged {
        subject
            .yield_releases
            .lock()
            .unwrap()
            .push(file.with_extension("release"));
    } else {
        let release = file.with_extension("release");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = std::fs::write(release, b"release");
        });
    }
    Ok(())
}
