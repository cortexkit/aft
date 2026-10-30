use super::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn isolated(name: &str, run: impl FnOnce()) {
    if std::env::var("AFT_S4_AUDIT_CHILD").as_deref() == Ok(name) {
        run();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("bash_background::secrets::tests::audit_followups::{name}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AFT_S4_AUDIT_CHILD", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture {
    root: tempfile::TempDir,
    _storage: tempfile::TempDir,
    ctx: Arc<AppContext>,
    vault: Arc<Vault>,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
    _launcher: Box<dyn std::any::Any>,
    tasks: Mutex<Vec<String>>,
}
impl Fixture {
    fn new() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let launcher = native_launcher(root.path());
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "AFT audit upstream CA");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
        let ca = params.self_signed(&key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .signed_by(&leaf_key, &ca, &key)
            .unwrap();
        let tls = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![leaf.der().clone()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
                )
                .unwrap(),
        );
        let pem = root.path().join("upstream.pem");
        std::fs::write(&pem, ca.pem()).unwrap();
        unsafe {
            std::env::set_var("SSL_CERT_FILE", &pem);
        }
        #[cfg(target_os = "macos")]
        {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(ca.der().clone()).unwrap();
            TEST_UPSTREAM.with(|slot| {
                slot.replace(Some(
                    rustls::ClientConfig::builder()
                        .with_root_certificates(roots)
                        .with_no_client_auth(),
                ))
            });
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let vault = Vault::new(listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen_server, stopping) = (seen.clone(), stop.clone());
        let server = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut stream = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(tls.clone()).unwrap(),
                    socket,
                );
                let mut bytes = Vec::new();
                loop {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    bytes.push(byte[0]);
                    if bytes.ends_with(b"\r\n\r\n") {
                        seen_server
                            .lock()
                            .unwrap()
                            .push(String::from_utf8(bytes).unwrap());
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        let _ = stream.flush();
                        break;
                    }
                }
            }
        });
        let ctx = Arc::new(context(root.path(), storage.path()));
        Self {
            root,
            _storage: storage,
            ctx,
            vault,
            seen,
            stop,
            server: Some(server),
            _launcher: Box::new(launcher),
            tasks: Mutex::new(Vec::new()),
        }
    }
    fn curl(&self) -> String {
        format!("curl --http1.1 --silent --show-error --fail --connect-timeout 1 --max-time 2 -H 'Authorization: Bearer {{{{ck-secret:test}}}}' https://localhost:{}/api", self.vault.port)
    }
    fn lifetime(&self, bound: bool) -> subc_protocol::scope::ScopeStamp {
        if !bound {
            return stamp("worker");
        }
        serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"audit-run","kind":"ephemeral","scope_epoch":1,"owner_authorized":true,"parent":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"secret-bash","scope_epoch":1},"parent_state":"linked"})).unwrap()
    }
    fn launch(&self, command: &str, bound: bool, extra: serde_json::Value) -> Response {
        let mut req = request(command);
        req.params["params"]
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let lifetime = self.lifetime(bound);
        let run = || {
            crate::bash_background::registry::with_scope_spawn(Some(lifetime.clone()), || {
                crate::commands::bash::handle(&req, &self.ctx)
            })
        };
        with_vault(self.vault.clone(), || {
            if bound {
                crate::subc::with_secret_bound_identity_for_test(
                    self.root.path(),
                    stamp("head"),
                    lifetime.clone(),
                    run,
                )
            } else {
                crate::subc::with_secret_identity_for_test(self.root.path(), lifetime.clone(), run)
            }
        })
    }
    fn wait_file(&self, name: &str) {
        let until = Instant::now() + Duration::from_secs(10);
        while !self.root.path().join(name).exists() {
            assert!(
                Instant::now() < until,
                "missing {name}: {}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait_requests(&self, count: usize) {
        wait(|| self.seen.lock().unwrap().len() >= count);
    }
    fn task(&self, response: &Response) -> String {
        assert!(response.success, "{response:?}");
        let id = response.data["task_id"].as_str().unwrap().to_owned();
        self.tasks.lock().unwrap().push(id.clone());
        id
    }
    fn diagnostics(&self) -> String {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .filter_map(|id| {
                self.ctx.bash_background().status(
                    id,
                    "secret-bash",
                    Some(self.root.path()),
                    Some(self._storage.path()),
                    8192,
                )
            })
            .map(|snapshot| {
                let output = |path: Option<String>| {
                    path.and_then(|path| std::fs::read_to_string(path).ok())
                        .unwrap_or_default()
                };
                format!(
                    "status={:?} exit={:?}\nstdout:{}\nstderr:{}",
                    snapshot.info.status,
                    snapshot.exit_code,
                    output(snapshot.output_path),
                    output(snapshot.stderr_path)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn wait_terminal(&self, id: &str) {
        wait(|| {
            self.ctx
                .bash_background()
                .status(
                    id,
                    "secret-bash",
                    Some(self.root.path()),
                    Some(self._storage.path()),
                    8192,
                )
                .is_some_and(|s| s.info.status.is_terminal())
        });
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.ctx.bash_background().shutdown();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}
fn wait(mut check: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < until, "condition did not become true");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn disconnected(address: SocketAddr) -> bool {
    TcpStream::connect(address).is_err()
}

#[test]
fn audit_authorizing_head_end_stops_run_proxy() {
    isolated("audit_authorizing_head_end_stops_run_proxy", || {
        let f = Fixture::new();
        let command = format!(
            "{}; : > first; while [ ! -f next ]; do sleep 0.05; done; {}; : > second; sleep 30",
            f.curl(),
            f.curl()
        );
        let id = f.task(&f.launch(&command, true, json!({})));
        f.wait_file("first");
        f.wait_requests(1);
        let address = f
            .ctx
            .bash_background()
            .secret_proxy_address_for_test(&id)
            .unwrap();
        assert!(
            f.ctx
                .bash_background()
                .live_scope_stamps()
                .iter()
                .any(|s| s == &stamp("head")),
            "head authorization missing from scope discovery"
        );
        f.ctx.bash_background().kill_scope(&stamp("head"));
        assert!(
            disconnected(address),
            "head ended while run listener stayed live"
        );
        std::fs::write(f.root.path().join("next"), b"go").unwrap();
        f.wait_file("second");
        assert_eq!(f.vault.materials.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn audit_relaxed_secret_plan_refuses() {
    isolated("audit_relaxed_secret_plan_refuses", || {
        let f = Fixture::new();
        f.ctx.update_config(|c| {
            c.sandbox.profile.head = crate::config::SandboxMode::Relaxed;
            c.sandbox.profile.worker = crate::config::SandboxMode::Relaxed;
        });
        let reply = f.launch("echo '{{ck-secret:test}}'", false, json!({}));
        assert_eq!(code(&reply), Some("secrets_sandbox_required"));
        assert_eq!(reply.data["message"], SANDBOX_REQUIRED);
        assert_eq!(f.vault.resolves.load(Ordering::SeqCst), 0);
    });
}

fn authorized(f: &Fixture) -> Launch {
    with_vault(f.vault.clone(), || {
        crate::subc::with_secret_identity_for_test(f.root.path(), stamp("worker"), || {
            authorize(
                "audit",
                &["test".into()],
                preflight(&f.ctx, "secret-bash", false, true).unwrap(),
                &HashMap::new(),
            )
            .unwrap_or_else(|r| panic!("{r:?}"))
        })
    })
}
#[test]
fn audit_task_limit_cleans_unarmed_proxy_and_scratch() {
    isolated("audit_task_limit_cleans_unarmed_proxy_and_scratch", || {
        let f = Fixture::new();
        f.ctx.update_config(|c| c.max_background_bash_tasks = 0);
        let launch = authorized(&f);
        let retained = launch.runtime.clone();
        let address = retained.lock().unwrap().address();
        let scratch = launch.preflight.plan.temp_dir().unwrap().to_owned();
        let reply = crate::bash_background::spawn_with_secrets(
            "audit",
            "secret-bash",
            "echo '{{ck-secret:test}}'",
            crate::bash_background::BashShell::default(),
            "/bin/sh".into(),
            Some(f.root.path().to_owned()),
            None,
            crate::bash_background::HardKill::Default,
            &f.ctx,
            true,
            true,
            false,
            false,
            24,
            80,
            vec![],
            false,
            None,
            None,
            Some(launch),
        );
        assert!(!reply.success);
        assert!(
            disconnected(address),
            "unarmed proxy survived task-limit return"
        );
        assert!(!scratch.exists(), "task-limit return leaked scratch");
    });
}
#[test]
fn audit_arm_failure_cleans_transferred_scratch() {
    isolated("audit_arm_failure_cleans_transferred_scratch", || {
        let f = Fixture::new();
        let mut launch = authorized(&f);
        let retained = launch.runtime.clone();
        let address = retained.lock().unwrap().address();
        let scratch = launch.preflight.plan.temp_dir().unwrap().to_owned();
        let task = launch.preflight.task.take().unwrap();
        assert_ne!(unsafe { libc::getsid(0) }, std::process::id() as i32);
        assert!(retained.lock().unwrap().arm(std::process::id()).is_err());
        let _ = crate::bash_background::persistence::delete_resolved_task(&task);
        drop(launch);
        assert!(
            disconnected(address),
            "failed arm retained an unarmed proxy"
        );
        assert!(!scratch.exists(), "failed arm leaked transferred scratch");
    });
}

#[test]
fn audit_killing_revokes_before_term_grace() {
    isolated("audit_killing_revokes_before_term_grace", || {
        let f = Fixture::new();
        let command = format!("trap ':' TERM; : > ready; while [ ! -f during-kill ]; do sleep 0.05; done; {}; : > term-request; while :; do sleep 0.05; done", f.curl());
        let id = f.task(&f.launch(&command, false, json!({})));
        f.wait_file("ready");
        let address = f
            .ctx
            .bash_background()
            .secret_proxy_address_for_test(&id)
            .unwrap();
        let (reached, release) =
            crate::bash_background::registry::install_kill_signal_gate_for_test(&id);
        let registry = f.ctx.bash_background().clone();
        let killed_id = id.clone();
        let kill = std::thread::spawn(move || registry.kill(&killed_id, "secret-bash"));
        reached.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            f.ctx
                .bash_background()
                .status(
                    &id,
                    "secret-bash",
                    Some(f.root.path()),
                    Some(f._storage.path()),
                    8192
                )
                .unwrap()
                .info
                .status,
            crate::bash_background::BgTaskStatus::Killing
        );
        let closed_while_alive = disconnected(address);
        std::fs::write(f.root.path().join("during-kill"), b"go").unwrap();
        f.wait_file("term-request");
        release.send(()).unwrap();
        let _ = kill.join().unwrap();
        assert!(closed_while_alive, "Killing still admits proxy connections");
        assert_eq!(
            f.vault.materials.load(Ordering::SeqCst),
            0,
            "command obtained fresh material after Killing"
        );
    });
}

#[test]
fn audit_bundle_does_not_copy_private_pem() {
    isolated("audit_bundle_does_not_copy_private_pem", || {
        let f = Fixture::new();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let combined = f.root.path().join("combined.pem");
        std::fs::write(
            &combined,
            format!("{}{}\nNOT A CERTIFICATE\n", cert.pem(), key.serialize_pem()),
        )
        .unwrap();
        unsafe {
            std::env::set_var("SSL_CERT_FILE", &combined);
        }
        let out = f.root.path().join("bundle.pem");
        write_bundle(&out, &cert.pem()).unwrap();
        let bytes = std::fs::read_to_string(out).unwrap();
        assert!(!bytes.contains("PRIVATE KEY"));
        assert!(!bytes.contains("NOT A CERTIFICATE"));
        assert!(bytes.contains("BEGIN CERTIFICATE"));
    });
}

#[test]
fn audit_runtime_disables_core_dumps() {
    isolated("audit_runtime_disables_core_dumps", || {
        let f = Fixture::new();
        let _launch = authorized(&f);
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) }, 0);
        assert_eq!(limit.rlim_cur, 0);
        assert_eq!(limit.rlim_max, 0);
        #[cfg(target_os = "linux")]
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE) }, 0);
    });
}

#[test]
fn audit_runtime_built_unscanned_label_is_refused() {
    isolated("audit_runtime_built_unscanned_label_is_refused", || {
        let f = Fixture::new();
        let command = format!(": '{{{{ck-secret:test}}}}'; H='{{{{ck-secret:un''scanned}}}}'; curl --http1.1 --silent --fail --max-time 2 -H \"Authorization: Bearer $H\" https://localhost:{}/api; code=$?; printf '%s' \"$code\" > result", f.vault.port);
        let id = f.task(&f.launch(&command, false, json!({})));
        f.wait_terminal(&id);
        assert_eq!(
            std::fs::read_to_string(f.root.path().join("result")).unwrap(),
            "22"
        );
        assert_eq!(f.vault.resolves.load(Ordering::SeqCst), 1);
        assert_eq!(f.vault.materials.load(Ordering::SeqCst), 0);
        assert!(f.seen.lock().unwrap().is_empty());
    });
}

#[test]
fn audit_natural_completion_removes_proxy() {
    isolated("audit_natural_completion_removes_proxy", || {
        completion_cleanup(false)
    });
}

#[test]
fn audit_timeout_removes_proxy() {
    isolated("audit_timeout_removes_proxy", || completion_cleanup(true));
}

fn completion_cleanup(timeout: bool) {
    let f = Fixture::new();
    let command = format!("printf '%s' \"${{SSL_CERT_FILE%/*}}\" > scratch-path; {}; : > first; while [ ! -f finish ]; do sleep 0.05; done", f.curl());
    let extra = if timeout {
        json!({"timeout":2500})
    } else {
        json!({})
    };
    let id = f.task(&f.launch(&command, false, extra));
    f.wait_file("first");
    let address = f
        .ctx
        .bash_background()
        .secret_proxy_address_for_test(&id)
        .unwrap();
    if !timeout {
        std::fs::write(f.root.path().join("finish"), b"go").unwrap();
    }
    f.wait_terminal(&id);
    wait(|| disconnected(address));
    wait(|| {
        f.ctx
            .bash_background()
            .secret_proxy_address_for_test(&id)
            .is_none()
    });
    let scratch = std::fs::read_to_string(f.root.path().join("scratch-path")).unwrap();
    // Completed commands retain ordinary scratch for later reads. Their secret
    // bundle and proxy ownership must still be released immediately.
    wait(|| !Path::new(&scratch).join("secret-trust.pem").exists());
    assert_eq!(f.vault.materials.load(Ordering::SeqCst), 1);
}

#[test]
fn audit_epoch_rebind_revokes_secret_not_ordinary_job() {
    isolated("audit_epoch_rebind_revokes_secret_not_ordinary_job", || {
        let f = Fixture::new();
        let id = f.task(&f.launch(
            &format!(
                "{}; : > first; while [ ! -f next ]; do sleep 0.05; done; {}; : > second; sleep 30",
                f.curl(),
                f.curl()
            ),
            true,
            json!({}),
        ));
        f.wait_file("first");
        f.wait_requests(1);
        let ordinary =
            crate::bash_background::registry::with_scope_spawn(Some(stamp("head")), || {
                crate::commands::bash::handle(&request("sleep 30"), &f.ctx)
            });
        let ordinary = f.task(&ordinary);
        let address = f
            .ctx
            .bash_background()
            .secret_proxy_address_for_test(&id)
            .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(crate::subc::confirm_secret_rebind_for_test(
                f.ctx.clone(),
                stamp("head"),
                &id,
            ));
        assert!(disconnected(address));
        std::fs::write(f.root.path().join("next"), b"go").unwrap();
        f.wait_file("second");
        assert_eq!(f.vault.materials.load(Ordering::SeqCst), 1);
        for task in [id, ordinary] {
            let snapshot = f
                .ctx
                .bash_background()
                .status(
                    &task,
                    "secret-bash",
                    Some(f.root.path()),
                    Some(f._storage.path()),
                    8192,
                )
                .unwrap();
            assert_eq!(
                snapshot.info.status,
                crate::bash_background::BgTaskStatus::Running,
                "rebind killed a command"
            );
            assert!(crate::bash_background::process::is_process_alive(
                snapshot.child_pid.unwrap()
            ));
        }
    });
}

#[test]
fn audit_hardened_daemon_preserves_child_fd_aliases() {
    isolated("audit_hardened_daemon_preserves_child_fd_aliases", || {
        let f = Fixture::new();
        let _protection = authorized(&f);
        let id = f.task(&f.launch("printf data | cat /dev/stdin", false, json!({})));
        f.wait_terminal(&id);
        let snapshot = f
            .ctx
            .bash_background()
            .status(
                &id,
                "secret-bash",
                Some(f.root.path()),
                Some(f._storage.path()),
                8192,
            )
            .unwrap();
        assert_eq!(snapshot.exit_code, Some(0), "{}", f.diagnostics());
        assert_eq!(
            std::fs::read(snapshot.output_path.unwrap()).unwrap(),
            b"data"
        );
    });
}
