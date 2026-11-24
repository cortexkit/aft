use super::*;
use crate::config::UserServerDef;

pub fn attempt_reason_for_test(result: &ServerAttemptResult) -> String {
    server_attempt_result_reason(result)
}
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const WAIT: Duration = Duration::from_secs(30);

thread_local! {
    pub(super) static MANAGER_LOCK_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn locked<R>(f: &Fixture, operation: impl FnOnce(&mut LspManager) -> R) -> R {
    let mut manager = f.manager.lock();
    MANAGER_LOCK_HELD.set(true);
    let result = operation(&mut manager);
    MANAGER_LOCK_HELD.set(false);
    result
}

pub struct Fixture {
    _root: tempfile::TempDir,
    pub(crate) config: Config,
    pub(crate) file: PathBuf,
    pub(crate) manager: Arc<parking_lot::Mutex<LspManager>>,
    pub(crate) snapshot: ApplicableServerSnapshot,
    pub(crate) key: ServerKey,
}

impl Fixture {
    pub fn assert_no_cached_failure(&self) {
        assert!(!self.manager.lock().failed_spawns.contains_key(&self.key));
        assert!(!self
            .manager
            .lock()
            .transient_backoff
            .contains_key(&self.key));
    }
}

pub fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("marker"), "marker").unwrap();
    let file = root.path().join("source.reserved");
    std::fs::write(&file, "content").unwrap();
    let mut frame = Vec::new();
    super::super::transport::write_message(
        &mut frame,
        r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
    )
    .unwrap();
    let script = root.path().join("server.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nIFS= read -r header\nprintf '%s' '{}'\nwhile IFS= read -r line; do :; done\n",
            String::from_utf8(frame).unwrap(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = Config {
        project_root: Some(root.path().to_path_buf()),
        lsp_servers: vec![UserServerDef {
            id: "reserved".into(),
            extensions: vec!["reserved".into()],
            binary: "reserved".into(),
            args: Vec::new(),
            root_markers: vec!["marker".into()],
            env: HashMap::new(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Custom(Arc::from("reserved")), script);
    let definition = servers_for_file(&file, &config).remove(0);
    let key = server_key_for_definition(&definition, &file, &config).unwrap();
    let snapshot = ApplicableServerSnapshot {
        server_keys: vec![key.clone()],
        candidates: vec![ApplicableServerCandidate {
            key: key.clone(),
            definition,
            source_file: file.clone(),
        }],
        ..ApplicableServerSnapshot::default()
    };
    let manager = Arc::new(parking_lot::Mutex::new(manager));
    manager.lock().startup_owner = Arc::downgrade(&manager);
    Fixture {
        _root: root,
        config,
        file,
        manager,
        snapshot,
        key,
    }
}

pub struct Pause {
    pub(crate) entered: Receiver<()>,
    pub(crate) resume: Sender<()>,
    pub(crate) spawns: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(super) under_lock: Arc<AtomicBool>,
}

pub fn pause_first_spawn(f: &Fixture) -> Pause {
    let (entered_tx, entered) = unbounded();
    let (resume, resume_rx) = unbounded();
    let spawns = Arc::new(AtomicUsize::new(0));
    let under_lock = Arc::new(AtomicBool::new(false));
    let count = Arc::clone(&spawns);
    let locked = Arc::clone(&under_lock);
    f.manager.lock().spawn_hook = Some(Arc::new(move || {
        // Only a guard held by the spawning thread violates the lock boundary;
        // another caller is allowed to inspect the manager during startup.
        if MANAGER_LOCK_HELD.get() {
            locked.store(true, Ordering::SeqCst);
        }
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            entered_tx.send(()).unwrap();
            resume_rx.recv_timeout(WAIT).unwrap();
        }
    }));
    Pause {
        entered,
        resume,
        spawns,
        #[cfg(test)]
        under_lock,
    }
}

impl Fixture {
    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn file(&self) -> &Path {
        &self.file
    }
    pub fn manager(&self) -> &parking_lot::Mutex<LspManager> {
        &self.manager
    }
    pub fn snapshot(&self) -> &ApplicableServerSnapshot {
        &self.snapshot
    }
    pub fn key(&self) -> &ServerKey {
        &self.key
    }
}

impl Pause {
    pub fn entered(&self) -> &Receiver<()> {
        &self.entered
    }
    pub fn resume(&self) -> &Sender<()> {
        &self.resume
    }
    pub fn spawns(&self) -> &AtomicUsize {
        &self.spawns
    }
}

impl LspManager {
    pub(crate) fn event_sender_for_test(&self) -> LspEventSender {
        LspEventSender::Managed {
            sender: self.event_tx.clone(),
            instance: ServerInstanceId::next(),
        }
    }

    #[doc(hidden)]
    pub fn insert_client_for_test(&mut self, client: LspClient) {
        let key = ServerKey {
            kind: client.kind(),
            root: client.root().to_path_buf(),
        };
        self.clients.insert(key, client);
    }
}

pub fn spawn_client_for_test(
    manager: &LspManager,
    kind: ServerKind,
    root: PathBuf,
    binary: &Path,
    args: &[String],
    registry: LspChildRegistry,
    note: Option<&str>,
) -> std::io::Result<(LspClient, u32)> {
    let mut client = LspClient::spawn_with_reclaim_root(
        kind,
        root,
        binary,
        args,
        &HashMap::new(),
        manager.event_sender_for_test(),
        registry,
        None,
        false,
    )?;
    client.runtime_note = note.map(str::to_owned);
    let pid = client.child_pid();
    Ok((client, pid))
}
