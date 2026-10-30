use super::*;
use crate::protocol::RawRequest;
use crate::secret::{Material, MaterialError, Resolution};
use secret_rules::location::{LabelRule, Location};
use serde_json::json;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[cfg(unix)]
#[path = "secrets_audit_tests.rs"]
mod audit_followups;

struct Vault {
    resolves: AtomicUsize,
    materials: AtomicUsize,
    port: u16,
    audits: Mutex<Vec<secret_rules::audit::AuditRecordV1>>,
    resolve_kinds: Mutex<Vec<String>>,
    material_kinds: Mutex<Vec<String>>,
    expected_epoch: AtomicU64,
}

impl Vault {
    fn new(port: u16) -> Arc<Self> {
        Arc::new(Self {
            resolves: AtomicUsize::new(0),
            materials: AtomicUsize::new(0),
            port,
            audits: Mutex::new(Vec::new()),
            resolve_kinds: Mutex::new(Vec::new()),
            material_kinds: Mutex::new(Vec::new()),
            expected_epoch: AtomicU64::new(1),
        })
    }
}

const KEY: &[u8] = b"s4-test-api-key-0123456789";

impl SecretVault for Vault {
    fn resolve(&self, scope: &crate::secret::SecretScope, label: &str) -> Resolution {
        #[cfg(unix)]
        {
            assert!(
                crate::secret_proxy::test_hardening::hardened_on_this_thread(),
                "hardening must precede vault resolution"
            );
            // The real limits change only in the audit tests' isolated child
            // processes; a shared test process keeps its limits.
            if std::env::var_os(crate::secret_proxy::test_hardening::REAL_ENV).is_some() {
                let mut core: libc::rlimit = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut core) }, 0);
                assert_eq!((core.rlim_cur, core.rlim_max), (0, 0));
                #[cfg(target_os = "linux")]
                assert_eq!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE) }, 0);
            }
        }
        assert_eq!(scope.reference(), "secret-bash");
        assert_eq!(scope.owner(), "reserved:prefrontal-core");
        assert_eq!(
            scope.stamp().scope_epoch,
            self.expected_epoch.load(Ordering::SeqCst)
        );
        self.resolves.fetch_add(1, Ordering::SeqCst);
        self.resolve_kinds
            .lock()
            .unwrap()
            .push(scope.kind().to_owned());
        Resolution::Granted {
            rule: LabelRule {
                label: label.into(),
                host: "localhost".into(),
                port: self.port,
                path_prefix: "/api".into(),
                location: Location::Bearer,
                grant_id: "grant".into(),
            },
            grant_id: "grant".into(),
            expires_at_ms: u64::MAX,
        }
    }
    fn material(
        &self,
        scope: &crate::secret::SecretScope,
        label: &str,
        grant_id: &str,
    ) -> Result<Material, MaterialError> {
        self.materials.fetch_add(1, Ordering::SeqCst);
        assert_eq!(scope.reference(), "secret-bash");
        assert_eq!(scope.owner(), "reserved:prefrontal-core");
        assert_eq!(
            scope.stamp().scope_epoch,
            self.expected_epoch.load(Ordering::SeqCst)
        );
        assert_eq!(label, "test");
        assert_eq!(grant_id, "grant");
        self.material_kinds
            .lock()
            .unwrap()
            .push(scope.kind().to_owned());
        Ok(Material::new(KEY.to_vec()))
    }
    fn audit(
        &self,
        record: &secret_rules::audit::AuditRecordV1,
    ) -> Result<(), crate::secret::AuditError> {
        self.audits.lock().unwrap().push(record.clone());
        Ok(())
    }
}

fn with_vault<R>(vault: Arc<dyn SecretVault>, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<dyn SecretVault>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_VAULT.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(TEST_VAULT.with(|slot| slot.replace(Some(vault))));
    run()
}

fn context(root: &Path, storage: &Path) -> AppContext {
    AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.to_owned()),
            storage_dir: Some(storage.to_owned()),
            experimental_bash_background: true,
            sandbox: crate::config::SandboxConfig {
                profile: crate::config::SandboxRoles {
                    head: crate::config::SandboxMode::Strict,
                    worker: crate::config::SandboxMode::Strict,
                },
                ..Default::default()
            },
            ..crate::config::Config::default()
        },
    )
}

fn request(command: &str) -> RawRequest {
    serde_json::from_value(json!({"id":"secret-bash", "command":"bash", "session_id":"secret-bash", "params":{"command":command,"background":true,"compressed":false}})).unwrap()
}

#[cfg(unix)]
fn stamp(kind: &str) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"secret-bash","kind":kind,"scope_epoch":1,"owner_authorized":true})).unwrap()
}

fn code(response: &Response) -> Option<&str> {
    response.data.get("code").and_then(|code| code.as_str())
}

#[test]
fn secret_bash_scan_and_environment_conflicts() {
    assert_eq!(
        labels("echo '{{ck-secret:a}} {{ck-secret:a}} {{ck-secret:b}} {{ck-secret:BAD}}'"),
        ["a", "b"]
    );
    for key in PROXY_KEYS.iter().chain(REMOVE_KEYS.iter()) {
        assert!(env_conflicts(&HashMap::from([(
            key.to_string(),
            String::new()
        )])));
    }
    assert!(!env_conflicts(&HashMap::from([(
        "OTHER".into(),
        "value".into()
    )])));
}

#[test]
fn secret_bash_remote_precedes_plan_scope_and_runon_validation() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path());
    let vault = Vault::new(443);
    let mut req = request("echo '{{ck-secret:test}}'");
    req.params["params"]["runon"] = json!("invalid-demand");
    let reply = with_vault(vault.clone(), || crate::commands::bash::handle(&req, &ctx));
    assert_eq!(code(&reply), Some("secrets_remote_refused"));
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
    assert_eq!(vault.audits.lock().unwrap().len(), 1);
    assert!(!std::fs::read_dir(storage.path())
        .unwrap()
        .any(|entry| entry.unwrap().file_name() == "tasks"));
}

#[cfg(unix)]
#[test]
fn secret_bash_legacy_remote_is_refused_before_any_job() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path());
    let launch = super::super::RemoteLaunch {
        explicit_runon: false,
        connection_file: None,
        harness: "broca".into(),
        session: "secret-bash".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
        configured_env_names: vec![],
        return_paths: Vec::new(),
        params: crate::exec_remote::FrozenParams {
            remote_exec: Some(crate::exec_remote::policy::RemoteExecPolicy {
                enabled: true,
                legacy_commands: Some(json!(["echo"])),
                ..Default::default()
            }),
            ..Default::default()
        },
    };
    let vault = Vault::new(443);
    let reply = with_vault(vault.clone(), || {
        super::super::with_remote_policy(Some(launch), || {
            crate::commands::bash::handle(&request("echo '{{ck-secret:test}}'"), &ctx)
        })
    });
    assert_eq!(code(&reply), Some("secrets_remote_refused"));
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
}

#[test]
fn secret_bash_nonqualifying_plans_precede_scope_and_rewrite() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path());
    let vault = Vault::new(443);
    let plans = [
        SpawnPlan::Unsandboxed,
        SpawnPlan::RelaxedUnenforced,
        SpawnPlan::Host {
            shell_path: "/bin/sh".into(),
            environment: Default::default(),
        },
    ];
    for plan in plans {
        let reply = with_vault(vault.clone(), || {
            crate::sandbox_spawn::with_spawn_plan_for_test(plan, || {
                crate::commands::bash::handle(
                    &request("echo '{{ck-secret:test}}' >> must-not-be-rewritten"),
                    &ctx,
                )
            })
        });
        assert_eq!(code(&reply), Some("secrets_sandbox_required"));
        assert!(!root.path().join("must-not-be-rewritten").exists());
    }
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
}

#[cfg(windows)]
#[test]
fn secret_bash_windows_refuses_all_profiles_without_a_job() {
    for mode in [
        crate::config::SandboxMode::Off,
        crate::config::SandboxMode::Relaxed,
        crate::config::SandboxMode::Strict,
    ] {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.path().to_owned());
        config.storage_dir = Some(storage.path().to_owned());
        config.sandbox.profile = crate::config::SandboxRoles {
            head: mode,
            worker: mode,
        };
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let vault = Vault::new(443);
        let reply = with_vault(vault.clone(), || {
            crate::commands::bash::handle(&request("echo '{{ck-secret:test}}'"), &ctx)
        });
        assert_eq!(code(&reply), Some("secrets_sandbox_required"));
        assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
    }
}

#[cfg(unix)]
fn native_launcher(root: &Path) -> impl Drop {
    let executable = std::env::current_exe().unwrap();
    let launcher = executable.parent().unwrap().parent().unwrap().join("aft");
    assert!(
        launcher.is_file(),
        "build the real aft binary before running native secret-bash tests"
    );
    crate::sandbox_spawn::install_sandbox_launcher_for_test(root, launcher)
}

#[cfg(unix)]
#[test]
fn secret_bash_real_plan_precedence_and_untrusted_request_scope() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let _launcher = native_launcher(root.path());
    let ctx = context(root.path(), storage.path());
    let req = request("echo '{{ck-secret:test}}'");
    let reply = crate::commands::bash::handle(&req, &ctx);
    assert_eq!(code(&reply), Some("secret_refused"), "{reply:?}");
    let mut forged = req.clone();
    forged.params["params"]["scope"] = serde_json::to_value(stamp("head")).unwrap();
    let vault = Vault::new(443);
    let reply = with_vault(vault.clone(), || {
        crate::commands::bash::handle(&forged, &ctx)
    });
    assert_eq!(code(&reply), Some("secret_refused"));
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
    let reply = crate::subc::with_secret_identity_for_test(root.path(), stamp("head"), || {
        crate::commands::bash::handle(&req, &ctx)
    });
    assert_eq!(code(&reply), Some("secrets_unavailable"));
    for key in PROXY_KEYS.iter().chain(REMOVE_KEYS.iter()) {
        let mut conflict = req.clone();
        conflict.params["params"]["env"] = json!({*key: "conflict"});
        let reply = with_vault(vault.clone(), || {
            crate::subc::with_secret_identity_for_test(root.path(), stamp("head"), || {
                crate::commands::bash::handle(&conflict, &ctx)
            })
        });
        assert_eq!(code(&reply), Some("secrets_env_conflict"));
    }
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
    ctx.update_config(|config| {
        config
            .bash
            .env
            .insert("NO_PROXY".into(), "localhost".into());
    });
    let reply = with_vault(vault.clone(), || crate::commands::bash::handle(&req, &ctx));
    assert_eq!(code(&reply), Some("secrets_env_conflict"));
}

#[cfg(unix)]
#[test]
fn secret_bash_real_pipeline_background_pty_environment_and_key_scan() {
    // A fresh process gives the platform TLS verifier its own native-root
    // environment without racing unrelated tests' process-wide TLS cache.
    const CHILD: &str = "AFT_SECRET_BASH_NATIVE_TEST";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "bash_background::secrets::tests::secret_bash_real_pipeline_background_pty_environment_and_key_scan", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    native_pipeline();
}

#[cfg(unix)]
fn native_pipeline() {
    use base64::Engine;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    let _ = rustls::crypto::ring::default_provider().install_default();
    use rustls::pki_types::pem::PemObject;
    let native = native_roots().unwrap();
    let first_native = rustls::pki_types::CertificateDer::pem_slice_iter(&native)
        .next()
        .expect("native roots must contain certificates")
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let _launcher = native_launcher(root.path());
    let expected_native = root.path().join("expected-native.b64");
    std::fs::write(
        &expected_native,
        base64::engine::general_purpose::STANDARD.encode(first_native.as_ref()),
    )
    .unwrap();
    let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "AFT bash fixture upstream CA");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = params.self_signed(&ca_key).unwrap();
    #[cfg(target_os = "macos")]
    {
        // Do not mutate the host keychains. This fixture tests the real bash
        // pipeline with a local upstream anchor, not macOS system-store trust.
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
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&leaf_key, &ca, &ca_key)
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
    let roots = root.path().join("roots.pem");
    std::fs::write(&roots, ca.pem()).unwrap();
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &roots);
        std::env::set_var("https_proxy", "http://127.0.0.1:1");
        std::env::set_var("NO_PROXY", "localhost");
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let vault = Vault::new(listener.local_addr().unwrap().port());
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_server = seen.clone();
    let cases = vec![
        (crate::config::SandboxMode::Strict, false),
        (crate::config::SandboxMode::Strict, true),
    ];
    let expected_requests = cases.len() * 2;
    let encodings = vec![
        std::str::from_utf8(KEY).unwrap().to_owned(),
        base64::engine::general_purpose::STANDARD.encode(KEY),
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(KEY),
        base64::engine::general_purpose::URL_SAFE.encode(KEY),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(KEY),
        KEY.iter().map(|byte| format!("{byte:02x}")).collect(),
        KEY.iter().map(|byte| format!("{byte:02X}")).collect(),
        KEY.iter().map(|byte| format!("%{byte:02X}")).collect(),
        KEY.iter().map(|byte| format!("%{byte:02x}")).collect(),
    ];
    let body = encodings.join("\n");
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while seen_server.load(Ordering::SeqCst) < expected_requests && Instant::now() < deadline {
            let (socket, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            // BSD can inherit the listener's O_NONBLOCK; TLS reads here block.
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(tls.clone()).unwrap(),
                socket,
            );
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.to_lowercase().contains(&format!(
                "authorization: bearer {}",
                std::str::from_utf8(KEY).unwrap()
            )));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            stream.flush().unwrap();
            seen_server.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(seen_server.load(Ordering::SeqCst), expected_requests);
    });
    let memory_probe = root.path().join("memory-probe.py");
    std::fs::write(&memory_probe, r#"import ctypes, errno, os, sys
pid = int(sys.argv[1])
if sys.platform == 'darwin':
    lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
    port = ctypes.c_uint(0)
    assert lib.task_for_pid(lib.mach_task_self(), pid, ctypes.byref(port)) != 0, 'daemon task port exposed'
else:
    try:
        f = open('/proc/%d/mem' % pid, 'rb')
    except OSError as error:
        # Restricted procfs mounts can hide an untraceable process as ENOENT.
        assert error.errno in (errno.EACCES, errno.EPERM, errno.ENOENT), error
    else:
        f.close()
        raise AssertionError('daemon memory exposed')
    assert ctypes.CDLL(None).ptrace(16, pid, 0, 0) == -1, 'daemon ptrace exposed'
paths = [os.environ[k] for k in ['SSL_CERT_FILE', 'CURL_CA_BUNDLE', 'NODE_EXTRA_CA_CERTS', 'REQUESTS_CA_BUNDLE', 'GIT_SSL_CAINFO']]
assert len(set(paths)) == 1
assert open(paths[0], 'rb').read().count(b'BEGIN CERTIFICATE') > 1
assert open(sys.argv[2], 'rb').read() in open(paths[0], 'rb').read().replace(b'\n', b'').replace(b'\r', b'')
"#).unwrap();
    let mut expected_origins = Vec::new();
    for (index, (mode, pty)) in cases.into_iter().enumerate() {
        vault
            .expected_epoch
            .store(if pty { 1 } else { index as u64 + 1 }, Ordering::SeqCst);
        #[cfg(target_os = "linux")]
        assert!(
            std::fs::metadata(format!("/proc/{}/mem", std::process::id())).is_ok(),
            "the target must exist outside the child sandbox"
        );
        let ctx = context(root.path(), storage.path());
        ctx.update_config(|config| {
            config.sandbox.profile.head = mode;
            config.sandbox.profile.worker = mode;
        });
        let gate = root.path().join(format!("go-{index}"));
        let command = format!("while [ ! -f '{}' ]; do sleep 0.05; done; test -n \"$HTTPS_PROXY\" && test \"$HTTPS_PROXY\" = \"$https_proxy\" && test \"$HTTP_PROXY\" = \"$http_proxy\" && test -z \"${{NO_PROXY+x}}\" && test -z \"${{no_proxy+x}}\" && test -z \"${{ALL_PROXY+x}}\" && test -z \"${{all_proxy+x}}\" || exit 70; printf 'proxy-env-ok\\n' >&2; curl --connect-timeout 5 --max-time 10 --silent --show-error --fail -H 'Authorization: Bearer {{{{ck-secret:test}}}}' https://localhost:{}/api; curl --connect-timeout 5 --max-time 10 --silent --show-error --fail -H 'Authorization: Bearer {{{{ck-secret:test}}}}' https://localhost:{}/api; sleep 30", gate.display(), vault.port, vault.port);
        let command = format!(
            "python3 '{}' {} '{}' || exit 71; printf 'memory-and-bundle-ok\\n' >&2; {command}",
            memory_probe.display(),
            std::process::id(),
            expected_native.display()
        );
        let mut req = request(&command);
        req.params["params"]["pty"] = json!(pty);
        req.params["params"]["env"] =
            json!({"SSL_CERT_FILE":"/must-not-read-tool-supplied-trust-path"});
        let mut lifetime = stamp("worker");
        lifetime.scope_epoch = index as u64 + 1;
        if pty {
            lifetime = serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"secret-run","kind":"ephemeral","scope_epoch":index as u64 + 1,"owner_authorized":true,"parent":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"secret-bash","scope_epoch":1},"parent_state":"linked"})).unwrap();
        }
        let launch = || {
            super::super::registry::with_scope_spawn(Some(lifetime.clone()), || {
                crate::commands::bash::handle(&req, &ctx)
            })
        };
        let response = with_vault(vault.clone(), || {
            if pty {
                crate::subc::with_secret_bound_identity_for_test(
                    root.path(),
                    stamp("head"),
                    lifetime.clone(),
                    launch,
                )
            } else {
                crate::subc::with_secret_identity_for_test(root.path(), lifetime.clone(), launch)
            }
        });
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap();
        expected_origins.extend([task_id.to_owned(), task_id.to_owned()]);
        let address = ctx
            .bash_background()
            .secret_proxy_address_for_test(task_id)
            .unwrap();
        assert_eq!(
            vault.resolves.load(Ordering::SeqCst),
            index + 1,
            "one resolve per distinct label, never per occurrence"
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            index * 2,
            "requests must wait until the launch returns"
        );
        std::fs::write(&gate, b"go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while seen.load(Ordering::SeqCst) < (index + 1) * 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let diagnostic = ctx
            .bash_background()
            .status(
                task_id,
                "secret-bash",
                Some(root.path()),
                Some(storage.path()),
                8192,
            )
            .map(|snapshot| {
                let output = |path: Option<String>| {
                    path.and_then(|path| std::fs::read_to_string(path).ok())
                        .unwrap_or_default()
                };
                format!(
                    "status={:?}, exit={:?}\nstdout:\n{}\nstderr:\n{}",
                    snapshot.info.status,
                    snapshot.exit_code,
                    output(snapshot.output_path),
                    output(snapshot.stderr_path)
                )
            })
            .unwrap_or_else(|| "task no longer registered".into());
        assert_eq!(
            seen.load(Ordering::SeqCst),
            (index + 1) * 2,
            "command failed before upstream:\n{diagnostic}"
        );
        let snapshot = loop {
            let snapshot = ctx
                .bash_background()
                .status(
                    task_id,
                    "secret-bash",
                    Some(root.path()),
                    Some(storage.path()),
                    8192,
                )
                .unwrap();
            assert!(!snapshot
                .output_preview
                .contains(std::str::from_utf8(KEY).unwrap()));
            let output = snapshot
                .output_path
                .as_ref()
                .and_then(|path| std::fs::read(path).ok())
                .unwrap_or_default();
            assert!(!output.windows(KEY.len()).any(|window| window == KEY));
            if String::from_utf8_lossy(&output)
                .matches("{{ck-secret:test}}")
                .count()
                >= encodings.len() * 2
            {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "proxy output missing: {snapshot:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let pid = snapshot.child_pid.unwrap() as i32;
        assert_eq!(
            unsafe { libc::getsid(pid) },
            pid,
            "command must lead its own session"
        );
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        if pty {
            ctx.bash_background().kill_scope(&lifetime);
        } else {
            ctx.bash_background().detach();
            assert!(
                super::super::process::is_process_alive(pid as u32),
                "detach should leave the child alive but revoke its proxy"
            );
            assert!(
                std::net::TcpStream::connect(address).is_err(),
                "detach left listener alive while child was alive"
            );
            assert!(super::super::process::is_process_alive(pid as u32));
            super::super::process::terminate_pgid(pid, None);
        }
        assert!(
            std::net::TcpStream::connect(address).is_err(),
            "ended scope kept proxy listener alive"
        );
    }
    server.join().unwrap();
    assert_eq!(vault.materials.load(Ordering::SeqCst), expected_requests);
    assert_eq!(
        *vault.resolve_kinds.lock().unwrap(),
        ["worker", "head"].repeat(expected_requests / 4)
    );
    assert_eq!(
        *vault.material_kinds.lock().unwrap(),
        ["worker", "worker", "head", "head"].repeat(expected_requests / 4)
    );
    let audits = vault.audits.lock().unwrap();
    assert_eq!(audits.len(), expected_requests);
    for (index, record) in audits.iter().enumerate() {
        let scope = record.scope.as_ref().unwrap();
        assert_eq!(scope.owner, "reserved:prefrontal-core");
        assert_eq!(scope.reference, "secret-bash");
        assert_eq!(
            scope.kind,
            if (index / 2) % 2 == 0 {
                "worker"
            } else {
                "head"
            }
        );
        assert_eq!(record.host.as_deref(), Some("localhost"));
        assert_eq!(record.port, Some(vault.port));
        assert_eq!(record.matched_prefix.as_ref().unwrap().path_prefix, "/api");
        assert_eq!(record.location_kind.as_deref(), Some("bearer"));
        assert_eq!(record.decision, secret_rules::audit::Decision::Substituted);
        assert_eq!(record.reason, secret_rules::audit::AuditReason::Matched);
        assert_eq!(record.label, "test");
        assert_eq!(record.origin, expected_origins[index]);
        assert!(!record.request_id.is_empty());
        let serialized = record.to_json_line_with_key(KEY).unwrap();
        for encoded in &encodings {
            assert!(!serialized.contains(encoded));
        }
    }
    fn scan(path: &Path, encodings: &[String]) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path, encodings);
            } else if let Ok(bytes) = std::fs::read(&path) {
                assert!(
                    !bytes.windows(KEY.len()).any(|window| window == KEY),
                    "key on disk at {}",
                    path.display()
                );
                for encoded in encodings {
                    assert!(
                        !bytes
                            .windows(encoded.len())
                            .any(|window| window == encoded.as_bytes()),
                        "encoded key on disk at {}",
                        path.display()
                    );
                }
                if path
                    .file_name()
                    .is_some_and(|name| name == "environment.bin")
                {
                    assert!(
                        !bytes.windows(7).any(|window| window == b"http://"),
                        "proxy credential persisted"
                    );
                }
            }
        }
    }
    scan(storage.path(), &encodings);
}
