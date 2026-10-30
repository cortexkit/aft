use super::*;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::AtomicUsize;

use hyper::header::HeaderValue;
use rustls::pki_types::pem::PemObject;
use secret_rules::location::Location;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::secret::vault::{AuditError, Material, MaterialError, Resolution};

const K1: &[u8] = b"test-key-12345678";
const K2: &[u8] = b"rotated-key-987654321";
const MARKER: &str = "{{ck-secret:test}}";
const SPECIAL: &[u8] = b"alpha&percent%quote\"slash\\\r\nkey";

struct Vault {
    calls: AtomicUsize,
    denied: AtomicBool,
    rotate: AtomicBool,
    refused_label: Mutex<Option<String>>,
    special: AtomicBool,
    audits: Mutex<Vec<secret_rules::audit::AuditRecordV1>>,
    block_material: AtomicBool,
    material_entered: tokio::sync::Notify,
    material_released: std::sync::Mutex<bool>,
    release_material: std::sync::Condvar,
}

impl Vault {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            denied: AtomicBool::new(false),
            rotate: AtomicBool::new(false),
            refused_label: Mutex::new(None),
            special: AtomicBool::new(false),
            audits: Mutex::new(Vec::new()),
            block_material: AtomicBool::new(false),
            material_entered: tokio::sync::Notify::new(),
            material_released: std::sync::Mutex::new(false),
            release_material: std::sync::Condvar::new(),
        })
    }
}

impl SecretVault for Vault {
    fn resolve(&self, _: &SecretScope, _: &str) -> Resolution {
        panic!("proxy must not resolve or file consent")
    }
    fn material(
        &self,
        scope: &SecretScope,
        label: &str,
        _: &str,
    ) -> Result<Material, MaterialError> {
        assert_eq!(scope.reference(), "head");
        let count = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.block_material.load(Ordering::Acquire) {
            self.material_entered.notify_one();
            let mut released = self.material_released.lock().unwrap();
            while !*released {
                released = self.release_material.wait(released).unwrap();
            }
        }
        if self.denied.load(Ordering::SeqCst) || self.refused_label.lock().as_deref() == Some(label)
        {
            return Err(MaterialError::GrantLapsed);
        }
        if self.special.load(Ordering::SeqCst) {
            return Ok(Material::new(SPECIAL.to_vec()));
        }
        Ok(Material::new(
            if self.rotate.load(Ordering::SeqCst) && count > 0 {
                K2
            } else {
                K1
            }
            .to_vec(),
        ))
    }
    fn audit(&self, record: &secret_rules::audit::AuditRecordV1) -> Result<(), AuditError> {
        self.audits.lock().push(record.clone());
        Ok(())
    }
}

fn scope(reference: &str) -> SecretScope {
    SecretScope::from_stamp(
        &serde_json::from_value(json!({
            "owner": {"kind":"reserved", "module_id":"prefrontal-core"},
        "owner_authorized":true, "kind":if reference == "run" {"ephemeral"} else {"head"}, "ref":reference, "scope_epoch":1
        }))
        .unwrap(),
    )
}

fn config(vault: Arc<Vault>, port: u16, locations: &[(&str, Location)]) -> ProxyConfig {
    ProxyConfig {
        vault,
        authorization_scope: scope("head"),
        lifetime_scope: scope("run"),
        scanned_labels: locations.iter().map(|(l, _)| (*l).to_owned()).collect(),
        grants: locations
            .iter()
            .map(|(label, location)| GrantedLabel {
                rule: LabelRule {
                    label: (*label).to_owned(),
                    host: "localhost".into(),
                    port,
                    path_prefix: "/approved".into(),
                    location: location.clone(),
                    grant_id: "grant".into(),
                },
                expires_at_ms: u64::MAX,
            })
            .collect(),
        origin: "test-task".into(),
    }
}

struct Server {
    address: SocketAddr,
    root_pem: String,
    client_tls: rustls::ClientConfig,
    seen: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
    handshake_seen: Arc<tokio::sync::Notify>,
    tls_release: Arc<tokio::sync::Notify>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn new(mode: &'static str) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
        let ca_key = KeyPair::generate().unwrap();
        let ca = params.self_signed(&ca_key).unwrap();
        let names = if mode == "wrong-name" {
            vec!["wrong.invalid".into()]
        } else {
            vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
        };
        let mut params = CertificateParams::new(names).unwrap();
        if mode == "expired" {
            params.not_before = rcgen::date_time_ymd(2000, 1, 1);
            params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        }
        let key = KeyPair::generate().unwrap();
        let leaf = params.signed_by(&key, &ca, &ca_key).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        tls.alpn_protocols = if mode.ends_with("-h1") {
            vec![b"http/1.1".to_vec()]
        } else {
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        };
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = if mode == "ipv6-h1" {
            TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0))
                .await
                .unwrap()
        } else {
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap()
        };
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let overlap = Arc::new(tokio::sync::Barrier::new(if mode == "hold" {
            32
        } else {
            2
        }));
        let handshake_seen = Arc::new(tokio::sync::Notify::new());
        let tls_release = Arc::new(tokio::sync::Notify::new());
        let seen_tls = handshake_seen.clone();
        let release_tls = tls_release.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                let recorded = recorded.clone();
                let overlap = overlap.clone();
                let seen_tls = seen_tls.clone();
                let release_tls = release_tls.clone();
                tokio::spawn(async move {
                    if mode == "delay-h1" {
                        seen_tls.notify_one();
                        release_tls.notified().await;
                    }
                    let Ok(tls) = acceptor.accept(socket).await else {
                        return;
                    };
                    if mode == "raw-transfer-h1" {
                        let mut tls = tls;
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") {
                            let Ok(byte) = tls.read_u8().await else {
                                return;
                            };
                            request.push(byte);
                        }
                        recorded
                            .lock()
                            .push(json!({"raw":String::from_utf8_lossy(&request)}));
                        let mut encoder = async_compression::tokio::bufread::GzipEncoder::new(K1);
                        let mut gzip = Vec::new();
                        encoder.read_to_end(&mut gzip).await.unwrap();
                        let wire = [b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\nConnection: close\r\n\r\n".to_vec(),
                            format!("{:x}\r\n",gzip.len()).into_bytes(),gzip,b"\r\n0\r\n\r\n".to_vec()].concat();
                        let _ = tls.write_all(&wire).await;
                        let _ = tls.shutdown().await;
                        return;
                    }
                    let service = service_fn(move |mut request: Request<Incoming>| {
                        let recorded = recorded.clone();
                        let overlap = overlap.clone();
                        async move {
                            let upgrade = if mode.starts_with("ws") {
                                Some(hyper::upgrade::on(&mut request))
                            } else {
                                None
                            };
                            let (parts, incoming) = request.into_parts();
                            let data = incoming.collect().await.unwrap().to_bytes();
                            let fields: HashMap<_, _> = parts
                                .headers
                                .iter()
                                .map(|(k, v)| {
                                    (
                                        k.to_string(),
                                        String::from_utf8_lossy(v.as_bytes()).to_string(),
                                    )
                                })
                                .collect();
                            recorded
                                .lock()
                                .push(json!({"target":parts.uri.to_string(),"headers":fields,
                                "body":String::from_utf8_lossy(&data)}));
                            if let Some(upgrade) = upgrade {
                                let mut response = Response::new(full(Bytes::new()));
                                *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                                response
                                    .headers_mut()
                                    .insert("connection", HeaderValue::from_static("upgrade"));
                                response
                                    .headers_mut()
                                    .insert("upgrade", HeaderValue::from_static("websocket"));
                                response.headers_mut().insert(
                                    "sec-websocket-accept",
                                    HeaderValue::from_static("fixture"),
                                );
                                if mode == "ws-extension" {
                                    response.headers_mut().insert(
                                        "sec-websocket-extensions",
                                        HeaderValue::from_static("permessage-deflate"),
                                    );
                                }
                                tokio::spawn(async move {
                                    if let Ok(upgraded) = upgrade.await {
                                        let mut io = TokioIo::new(upgraded);
                                        let frames = if mode == "ws-encodings" {
                                            let payload = [
                                                b"prefix|".repeat(2000),
                                                representations(K1),
                                                b"|tail".to_vec(),
                                            ]
                                            .concat();
                                            let chunks = payload.chunks(7).collect::<Vec<_>>();
                                            let mut frames = Vec::new();
                                            for (index, chunk) in chunks.iter().enumerate() {
                                                frames.push(
                                                    (if index + 1 == chunks.len() {
                                                        0x80
                                                    } else {
                                                        0
                                                    }) | if index == 0 { 1 } else { 0 },
                                                );
                                                frames.push(chunk.len() as u8);
                                                frames.extend_from_slice(chunk);
                                            }
                                            frames
                                        } else if mode.starts_with("ws-rsv") {
                                            let bit = match mode {
                                                "ws-rsv2" => 0x20,
                                                "ws-rsv3" => 0x10,
                                                _ => 0x40,
                                            };
                                            [vec![0x81 | bit, K1.len() as u8], K1.to_vec()].concat()
                                        } else {
                                            [
                                                vec![0x01, 8],
                                                K1[..8].to_vec(),
                                                vec![0x80, (K1.len() - 8) as u8],
                                                K1[8..].to_vec(),
                                            ]
                                            .concat()
                                        };
                                        let _ = io.write_all(&frames).await;
                                        let _ = io.shutdown().await;
                                    }
                                });
                                return Ok::<_, std::convert::Infallible>(response);
                            }
                            if mode == "overlap" || mode == "hold" {
                                overlap.wait().await;
                            }
                            let mut payload = if mode.starts_with("echo-request") {
                                [
                                    parts.uri.to_string().as_bytes(),
                                    b"|",
                                    &data,
                                    b"|",
                                    parts
                                        .headers
                                        .get("authorization")
                                        .map(|v| v.as_bytes())
                                        .unwrap_or(b""),
                                ]
                                .concat()
                            } else {
                                representations(K1)
                            };
                            if mode == "overlap" {
                                payload.extend_from_slice(K2);
                            }
                            let payload = match mode {
                                "gzip" => {
                                    let mut encoder =
                                        async_compression::tokio::bufread::GzipEncoder::new(
                                            payload.as_slice(),
                                        );
                                    let mut out = Vec::new();
                                    encoder.read_to_end(&mut out).await.unwrap();
                                    out
                                }
                                "deflate" => {
                                    let mut encoder =
                                        async_compression::tokio::bufread::ZlibEncoder::new(
                                            payload.as_slice(),
                                        );
                                    let mut out = Vec::new();
                                    encoder.read_to_end(&mut out).await.unwrap();
                                    out
                                }
                                "br" => {
                                    let mut encoder =
                                        async_compression::tokio::bufread::BrotliEncoder::new(
                                            payload.as_slice(),
                                        );
                                    let mut out = Vec::new();
                                    encoder.read_to_end(&mut out).await.unwrap();
                                    out
                                }
                                _ => payload,
                            };
                            let mut trailers = hyper::HeaderMap::new();
                            trailers.insert("x-trailer", HeaderValue::from_bytes(K1).unwrap());
                            let mut frames: Vec<Result<_, std::io::Error>> = payload
                                .chunks(3)
                                .map(|chunk| {
                                    Ok(hyper::body::Frame::data(Bytes::copy_from_slice(chunk)))
                                })
                                .collect();
                            frames.push(Ok(hyper::body::Frame::trailers(trailers)));
                            let mut response = Response::new(
                                http_body_util::StreamBody::new(futures_util::stream::iter(frames))
                                    .boxed_unsync(),
                            );
                            response
                                .headers_mut()
                                .insert("x-echo", HeaderValue::from_bytes(K1).unwrap());
                            response
                                .headers_mut()
                                .insert("trailer", HeaderValue::from_static("x-trailer"));
                            if ["gzip", "deflate", "br", "unknown"].contains(&mode) {
                                response
                                    .headers_mut()
                                    .insert("content-encoding", HeaderValue::from_static(mode));
                            }
                            Ok::<_, std::convert::Infallible>(response)
                        }
                    });
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Self {
            address,
            root_pem: ca.pem(),
            client_tls,
            seen,
            task,
            handshake_seen,
            tls_release,
        }
    }
}

fn representations(key: &[u8]) -> Vec<u8> {
    let mut payload = key.to_vec();
    for offset in 0..3 {
        let mut data = vec![0x9a; offset];
        data.extend_from_slice(key);
        data.extend_from_slice(b"suffix");
        for alphabet in [
            &base64::engine::general_purpose::STANDARD,
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        ] {
            payload.push(b'|');
            payload.extend_from_slice(alphabet.encode(&data).as_bytes());
        }
    }
    payload.push(b'|');
    payload.extend_from_slice(
        key.iter()
            .map(|b| format!("%{b:02x}"))
            .collect::<String>()
            .as_bytes(),
    );
    payload.push(b'|');
    payload.extend_from_slice(
        key.iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>()
            .as_bytes(),
    );
    payload.push(b'|');
    payload.extend_from_slice(
        key.iter()
            .map(|b| format!("\\u{b:04x}"))
            .collect::<String>()
            .as_bytes(),
    );
    payload
}

struct Client {
    process: Child,
    input: ChildStdin,
    output: std::io::BufReader<ChildStdout>,
}

impl Client {
    fn spawn(proxy: &SecretProxy, server: &Server) -> Self {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "secret_proxy::tests::client_child",
                "--nocapture",
            ])
            .env("AFT_SECRET_PROXY_CHILD", "1")
            .env("AFT_PROXY", proxy.proxy_url().as_str())
            .env("AFT_PROXY_CA", proxy.ca_pem())
            .env("AFT_UPSTREAM_CA", &server.root_pem)
            .env(
                "AFT_UPSTREAM",
                format!("localhost:{}", server.address.port()),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut process = command.spawn().unwrap();
        Self {
            input: process.stdin.take().unwrap(),
            output: std::io::BufReader::new(process.stdout.take().unwrap()),
            process,
        }
    }

    fn command(&mut self, value: Value) -> Value {
        writeln!(self.input, "{value}").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                self.output.read_line(&mut line).unwrap(),
                0,
                "client exited: {:?}",
                self.process.try_wait()
            );
            if let Some(value) = child_reply(&line) {
                return value;
            }
        }
    }

    fn connect(&mut self, h2: bool) {
        assert_eq!(self.command(json!({"op":"connect","h2":h2}))["ok"], true);
    }
    fn request(&mut self, path: &str, headers: Value, body: &str) -> Value {
        self.command(json!({"op":"request","path":path,"headers":headers,"body":body}))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[test]
fn client_child() {
    if std::env::var_os("AFT_SECRET_PROXY_CHILD").is_none() {
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        enum Sender {
            H1(hyper::client::conn::http1::SendRequest<http_body_util::Full<Bytes>>),
            H2(hyper::client::conn::http2::SendRequest<http_body_util::Full<Bytes>>),
        }
        let mut senders: Vec<Sender> = Vec::new();
        let mut raw_sockets = Vec::new();
        for line in std::io::stdin().lock().lines() {
            let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                if value["op"] == "raw" {
                    let socket: &mut tokio_rustls::client::TlsStream<TcpStream> = raw_sockets.last_mut().ok_or(())?;
                    socket.write_all(value["wire"].as_str().ok_or(())?.as_bytes()).await.map_err(|_| ())?;
                    let mut response = Vec::new(); socket.read_to_end(&mut response).await.map_err(|_| ())?;
                    return Ok(json!({"wire":String::from_utf8_lossy(&response)}));
                }
                if value["op"] == "idle" {
                    let socket = raw_sockets.last_mut().ok_or(())?;
                    let closed = matches!(tokio::time::timeout(Duration::from_millis(1000), socket.read_u8()).await, Ok(Err(_)));
                    return Ok(json!({"closed":closed}));
                }
                if value["op"] == "cap_connections" {
                    let proxy = url::Url::parse(&std::env::var("AFT_PROXY").unwrap()).unwrap();
                    let authority = std::env::var("AFT_UPSTREAM").unwrap();
                    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{}:",proxy.username()));
                    let mut held = Vec::new();
                    for _ in 0..18 {
                        let mut socket = TcpStream::connect((proxy.host_str().unwrap(),proxy.port().unwrap())).await.map_err(|_| ())?;
                        if socket.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Authorization: Basic {credentials}\r\n\r\n").as_bytes()).await.is_err() { continue; }
                        let mut header = Vec::new();
                        while !header.ends_with(b"\r\n\r\n") { match socket.read_u8().await {Ok(b)=>header.push(b),Err(_)=>break} }
                        if header.starts_with(b"HTTP/1.1 200") { held.push(socket); }
                    }
                    return Ok(json!({"accepted":held.len()}));
                }
                if value["op"] == "escaped" {
                    use std::os::unix::process::CommandExt;
                    let mut command = Command::new(std::env::current_exe().unwrap());
                    command.args(["--exact", "secret_proxy::tests::client_child", "--nocapture"])
                        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
                    unsafe { command.pre_exec(|| { if libc::setsid() < 0 { return Err(std::io::Error::last_os_error()); } Ok(()) }); }
                    let mut process = command.spawn().map_err(|_| ())?;
                    writeln!(process.stdin.take().unwrap(), "{{\"op\":\"connect\"}}").map_err(|_| ())?;
                    let output = process.wait_with_output().map_err(|_| ())?;
                    return Ok(String::from_utf8_lossy(&output.stdout).lines().find_map(child_reply).ok_or(())?);
                }
                if value["op"] == "connect" {
                    let proxy = url::Url::parse(&std::env::var("AFT_PROXY").unwrap()).unwrap();
                    let authority = value["authority"].as_str().map(str::to_owned).unwrap_or_else(|| std::env::var("AFT_UPSTREAM").unwrap());
                    let mut socket = TcpStream::connect((proxy.host_str().unwrap(), proxy.port().unwrap())).await.map_err(|_| ())?;
                    let token = if value["wrong_token"] == true { "wrong-token" } else { proxy.username() };
                    let host = value["connect_host"].as_str().unwrap_or(&authority);
                    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
                    socket.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {host}\r\nProxy-Authorization: Basic {credentials}\r\n\r\n").as_bytes()).await.map_err(|_| ())?;
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") { header.push(socket.read_u8().await.map_err(|_| ())?); }
                    if !header.starts_with(b"HTTP/1.1 200") { return Err(()); }
                    let mut roots = rustls::RootCertStore::empty();
                    for name in ["AFT_PROXY_CA", "AFT_UPSTREAM_CA"] {
                        if value["trust_upstream_only"] == true && name == "AFT_PROXY_CA" { continue; }
                        for cert in rustls::pki_types::CertificateDer::pem_slice_iter(std::env::var(name).unwrap().as_bytes()) { roots.add(cert.unwrap()).unwrap(); }
                    }
                    let mut config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
                    let h2 = value["h2"] == true;
                    config.alpn_protocols = vec![if h2 { b"h2".to_vec() } else { b"http/1.1".to_vec() }];
                    let host = authority.rsplit_once(':').unwrap().0.trim_start_matches('[').trim_end_matches(']').to_owned();
                    let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(rustls::pki_types::ServerName::try_from(host).unwrap(), socket).await.map_err(|_| ())?;
                    if value["raw"] == true { raw_sockets.push(tls); return Ok(json!({"ok":true})); }
                    if h2 {
                        let (sender, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls)).await.map_err(|_| ())?;
                        tokio::spawn(async move { let _ = connection.await; }); senders.push(Sender::H2(sender));
                    } else {
                        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await.map_err(|_| ())?;
                        tokio::spawn(async move { let _ = connection.with_upgrades().await; }); senders.push(Sender::H1(sender));
                    }
                    return Ok(json!({"ok":true}));
                }
                let index = value["connection"].as_u64().map(|v| v as usize).unwrap_or(senders.len().saturating_sub(1));
                let sender = senders.get_mut(index).ok_or(())?;
                if value["op"] == "many" {
                    let Sender::H2(sender) = sender else { return Err(()); };
                    let uri = format!("https://{}/approved",std::env::var("AFT_UPSTREAM").unwrap());
                    let futures = (0..36).map(|_| {
                        let mut sender = sender.clone(); let uri = uri.clone();
                        async move { let request = Request::builder().uri(uri).header("authorization",format!("Bearer {MARKER}")).body(Full::new(Bytes::new())).unwrap();
                            let response = sender.send_request(request).await.map_err(|_| ())?; let status = response.status().as_u16(); response.into_body().collect().await.map_err(|_| ())?; Ok::<_,()>(status) }
                    });
                    return Ok(json!({"statuses":futures_util::future::join_all(futures).await.into_iter().collect::<Result<Vec<_>,_>>()?}));
                }
                if value["op"] == "overlap" {
                    let Sender::H2(sender) = sender else { return Err(()); };
                    let mut second = sender.clone();
                    let uri = format!("https://{}/approved", std::env::var("AFT_UPSTREAM").unwrap());
                    let make = || Request::builder().uri(&uri).header("authorization",format!("Bearer {MARKER}"))
                        .body(Full::new(Bytes::new())).unwrap();
                    let (a, b) = tokio::join!(sender.send_request(make()), second.send_request(make()));
                    let (a, b) = tokio::join!(a.map_err(|_| ())?.into_body().collect(), b.map_err(|_| ())?.into_body().collect());
                    return Ok(json!({"a":String::from_utf8_lossy(&a.map_err(|_| ())?.to_bytes()),"b":String::from_utf8_lossy(&b.map_err(|_| ())?.to_bytes())}));
                }
                let authority = value["authority"].as_str().map(str::to_owned).unwrap_or_else(|| std::env::var("AFT_UPSTREAM").unwrap());
                let path = value["path"].as_str().unwrap_or("/approved");
                let uri = if matches!(sender, Sender::H2(_)) { format!("https://{authority}{path}") } else { path.to_owned() };
                let mut request = Request::builder().uri(uri).method("POST").header("te", "trailers");
                if value["omit_host"] != true { request = request.header("host", value["host_override"].as_str().unwrap_or(&authority)); }
                if let Some(headers) = value["headers"].as_object() {
                    for (key, val) in headers { request = request.header(key, val.as_str().unwrap()); }
                }
                let request = request.body(Full::new(Bytes::copy_from_slice(value["body"].as_str().unwrap_or("").as_bytes()))).unwrap();
                let mut response = match sender {
                    Sender::H1(sender) => sender.send_request(request).await.map_err(|_| ())?,
                    Sender::H2(sender) => sender.send_request(request).await.map_err(|_| ())?,
                };
                let status = response.status().as_u16();
                let headers: HashMap<_, _> = response.headers().iter().map(|(n,v)| (n.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string())).collect();
                if status == 101 {
                    let upgraded = hyper::upgrade::on(&mut response).await.map_err(|_| ())?;
                    let mut io = TokioIo::new(upgraded); let mut frames = Vec::new();
                    let _ = io.read_to_end(&mut frames).await;
                    return Ok(json!({"status":status,"frames":base64::engine::general_purpose::STANDARD.encode(frames)}));
                }
                let collected = response.into_body().collect().await.map_err(|_| ())?;
                let trailers: HashMap<_,_> = collected.trailers().map(|h| h.iter().map(|(n,v)| (n.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string())).collect()).unwrap_or_default();
                let bytes = collected.to_bytes();
                Ok(json!({"status":status,"headers":headers,"body":String::from_utf8_lossy(&bytes),"bytes":base64::engine::general_purpose::STANDARD.encode(&bytes),"trailers":trailers}))
            }).await;
            println!("{}", result.ok().and_then(Result::ok).unwrap_or(json!({"error":true})));
            std::io::stdout().flush().unwrap();
        }
    });
}

#[path = "audit_tests.rs"]
mod audit_tests;

fn child_reply(line: &str) -> Option<Value> {
    // With a single test thread, libtest prefixes uncaptured output with the
    // test name on the same line. Only the following JSON is our IPC reply.
    serde_json::from_str(line.get(line.find('{')?..)?.trim()).ok()
}

#[test]
fn child_reply_accepts_libtest_single_thread_prefix() {
    assert_eq!(
        child_reply("test client_child ... {\"ok\":true}\n"),
        Some(json!({"ok":true}))
    );
    assert_eq!(
        child_reply("{\"status\":403}\n"),
        Some(json!({"status":403}))
    );
    assert_eq!(child_reply("running 1 test\n"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_h1_h2_locations_keepalive_authority_and_scanned_set() {
    let server = Server::new("echo-request").await;
    let vault = Vault::new();
    let cfg = config(
        vault.clone(),
        server.address.port(),
        &[
            ("test", Location::Bearer),
            ("query", Location::Query("key".into())),
            ("json", Location::Json("/a~1b/~0".into())),
            ("header", Location::Header("x-api-key".into())),
        ],
    );
    let mut proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    for h2 in [false, true] {
        client.connect(h2);
        let response = client.request("/approved/./one?key=%7B%7Bck-secret%3Aquery%7D%7D", json!({
            "authorization":format!("Bearer {MARKER}"), "x-api-key":"{{ck-secret:header}}", "content-type":"application/json"
        }), r#"{"a/b":{"~":"{{ck-secret:json}}"},"literal":"{{ck-secret:unscanned}}"}"#);
        assert_eq!(response["status"], 200, "{response}");
        let seen = server.seen.lock().last().unwrap().clone();
        assert!(seen["target"]
            .as_str()
            .unwrap()
            .contains("/approved/one?key=test-key-12345678"));
        assert_eq!(seen["headers"]["authorization"], "Bearer test-key-12345678");
        assert_eq!(seen["headers"]["x-api-key"], "test-key-12345678");
        assert_eq!(seen["headers"]["accept-encoding"], "identity");
        assert_eq!(
            seen["body"],
            r#"{"a/b":{"~":"test-key-12345678"},"literal":"{{ck-secret:unscanned}}"}"#
        );
        // The receiving HPACK decoder does not preserve the never-indexed bit
        // as HeaderValue sensitivity. Check the actual outbound annotations.
        assert!(proxy.state.headers_sensitive.load(Ordering::Acquire));
        assert!(!response
            .to_string()
            .contains(std::str::from_utf8(K1).unwrap()));
        // Reuse the connection: approval belongs to each request, not a socket.
        let response = client.request(
            "/outside",
            json!({"authorization":format!("Bearer {MARKER}")}),
            "",
        );
        assert_eq!(response["status"], 200, "{response}");
        assert_eq!(
            server.seen.lock().last().unwrap()["headers"]["authorization"],
            format!("Bearer {MARKER}")
        );
        assert_eq!(client.request("/approved/two", json!({"authorization":format!("Bearer {MARKER}"),"content-type":"application/json"}), "{}")["status"], 200);
        let response = client.command(json!({"op":"request", "authority":"other.invalid", "path":"/approved", "headers":{"authorization":format!("Bearer {MARKER}")}}));
        assert_eq!(response["status"], 403, "{response}");
    }
    assert_eq!(vault.calls.load(Ordering::SeqCst), 16);
    proxy.stop();
    assert!(proxy.state.patterns.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_scrubs_encodings_headers_trailers_and_unexpected_compression() {
    for mode in ["plain", "gzip", "deflate", "br", "unknown"] {
        let server = Server::new(mode).await;
        let vault = Vault::new();
        let proxy = SecretProxy::start_with_tls(
            config(
                vault.clone(),
                server.address.port(),
                &[("test", Location::Bearer)],
            ),
            server.client_tls.clone(),
        )
        .unwrap();
        let mut client = Client::spawn(&proxy, &server);
        proxy.arm_command(client.process.id()).unwrap();
        for h2 in [false, true] {
            client.connect(h2);
            let response = client.request(
                "/approved",
                json!({"authorization":format!("Bearer {MARKER}")}),
                "",
            );
            if mode == "unknown" {
                assert_eq!(response["status"], 403);
                continue;
            }
            assert_eq!(response["status"], 200, "{mode}: {response}");
            assert_eq!(response["headers"]["x-echo"], MARKER);
            assert_eq!(response["headers"]["content-encoding"], Value::Null);
            assert_eq!(response["headers"]["content-length"], Value::Null);
            assert_eq!(
                response["trailers"]["x-trailer"], MARKER,
                "{mode}: {response}"
            );
            let body = response["body"].as_str().unwrap();
            assert_eq!(body.matches(MARKER).count(), 10, "{mode}: {body}");
            for encoding in [
                String::from_utf8(K1.to_vec()).unwrap(),
                base64::engine::general_purpose::STANDARD.encode(K1),
                K1.iter().map(|b| format!("{b:02X}")).collect(),
            ] {
                assert!(!response.to_string().contains(&encoding));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_literal_responses_only_gain_matchers_after_material() {
    let server = Server::new("plain").await;
    let vault = Vault::new();
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let before = client.request("/outside", json!({}), "");
    assert!(before["body"]
        .as_str()
        .unwrap()
        .contains(std::str::from_utf8(K1).unwrap()));
    assert_eq!(vault.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        200
    );
    let after = client.request(
        "/outside",
        json!({"authorization":"Bearer {{ck-secret:from-file}}"}),
        "",
    );
    assert!(!after.to_string().contains(std::str::from_utf8(K1).unwrap()));
    assert_eq!(
        server.seen.lock().last().unwrap()["headers"]["authorization"],
        "Bearer {{ck-secret:from-file}}"
    );
    assert_eq!(vault.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_atomic_json_and_material_refusals_send_nothing_upstream() {
    let server = Server::new("plain").await;
    let vault = Vault::new();
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[
                ("test", Location::Bearer),
                ("json", Location::Json("/key".into())),
            ],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    for body in [
        r#"{"key":4}"#,
        r#"{"key":"{{ck-secret:json}}","key":"{{ck-secret:json}}"}"#,
    ] {
        assert_eq!(client.request("/approved", json!({"authorization":format!("Bearer {MARKER}"),"content-type":"application/json"}), body)["status"], 403);
    }
    assert_eq!(vault.calls.load(Ordering::SeqCst), 0);
    assert!(server.seen.lock().is_empty());
    *vault.refused_label.lock() = Some("json".into());
    let refused = client.request(
        "/approved",
        json!({"authorization":format!("Bearer {MARKER}"),"content-type":"application/json"}),
        r#"{"key":"{{ck-secret:json}}"}"#,
    );
    assert_eq!(refused["status"], 403);
    assert_eq!(
        refused["body"],
        "Secret proxy refused the request: json (localhost)\n"
    );
    assert_eq!(vault.calls.load(Ordering::SeqCst), 2);
    assert!(server.seen.lock().is_empty());
    *vault.refused_label.lock() = None;
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}"),"content-type":"application/json"}),
            r#"{"key":"{{ck-secret:json}}"}"#
        )["status"],
        200
    );
    vault.denied.store(true, Ordering::SeqCst);
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}"),"content-type":"application/json"}),
            r#"{"key":"{{ck-secret:json}}"}"#
        )["status"],
        403
    );
    assert_eq!(server.seen.lock().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_upstream_tls_refuses_wrong_name_expiry_and_untrusted_root() {
    for mode in ["wrong-name", "expired", "untrusted"] {
        let server = Server::new(mode).await;
        let vault = Vault::new();
        let cfg = config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        );
        let proxy = if mode == "untrusted" {
            SecretProxy::start(cfg).unwrap()
        } else {
            SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap()
        };
        let mut client = Client::spawn(&proxy, &server);
        proxy.arm_command(client.process.id()).unwrap();
        client.connect(false);
        let response = client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            "",
        );
        assert_eq!(response["status"], 403, "{mode}: {response}");
        assert!(server.seen.lock().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_websocket_scrubs_fragments_and_refuses_extensions_and_rsv() {
    for mode in [
        "ws",
        "ws-extension",
        "ws-rsv",
        "ws-rsv2",
        "ws-rsv3",
        "ws-encodings",
    ] {
        let server = Server::new(mode).await;
        let vault = Vault::new();
        let proxy = SecretProxy::start_with_tls(
            config(vault, server.address.port(), &[("test", Location::Bearer)]),
            server.client_tls.clone(),
        )
        .unwrap();
        let mut client = Client::spawn(&proxy, &server);
        proxy.arm_command(client.process.id()).unwrap();
        client.connect(false);
        let response = client.request("/approved", json!({"authorization":format!("Bearer {MARKER}"),"connection":"upgrade","upgrade":"websocket","sec-websocket-extensions":"permessage-deflate"}), "");
        assert!(server.seen.lock()[0]["headers"]
            .get("sec-websocket-extensions")
            .is_none());
        if mode == "ws-extension" {
            assert_eq!(response["status"], 403);
            assert_eq!(client.request("/approved", json!({}), "")["error"], true);
        } else {
            assert_eq!(response["status"], 101, "{response}");
            let frames = base64::engine::general_purpose::STANDARD
                .decode(response["frames"].as_str().unwrap())
                .unwrap();
            if mode == "ws-encodings" {
                let message = ws_message(&frames);
                let text = std::str::from_utf8(&message).unwrap();
                assert!(text.starts_with("prefix|prefix|"));
                assert!(text.ends_with("|tail"));
                assert_eq!(text.matches(MARKER).count(), 10);
                assert!(!text.contains(std::str::from_utf8(K1).unwrap()));
            } else if mode.starts_with("ws-rsv") {
                assert!(frames.is_empty());
            } else {
                assert_eq!(
                    frames,
                    [vec![0x81, MARKER.len() as u8], MARKER.as_bytes().to_vec()].concat()
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_non_rule_connect_is_tunnelled_without_interception() {
    let server = Server::new("echo-request").await;
    let vault = Vault::new();
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    assert_eq!(
        client.command(json!({"op":"connect","authority":server.address.to_string(),"trust_upstream_only":true}))["ok"],
        true
    );
    let response = client.request(
        "/approved",
        json!({"authorization":"Bearer {{ck-secret:from-file}}"}),
        "",
    );
    assert_eq!(response["status"], 200);
    assert_eq!(
        server.seen.lock()[0]["headers"]["authorization"],
        "Bearer {{ck-secret:from-file}}"
    );
    assert_eq!(vault.calls.load(Ordering::SeqCst), 0);
    assert!(proxy.state.patterns.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_scope_stop_closes_listener_and_open_connections() {
    let server = Server::new("plain").await;
    let vault = Vault::new();
    let mut proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(true);
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        200
    );
    proxy.stop_if_scope_ended(&scope("head"));
    assert!(!proxy.state.stopped.load(Ordering::Acquire));
    // Hold a connection the proxy accepts but never serves: the CONNECT
    // headers stay incomplete, so no dispatch (and no vault call) can
    // happen before the stop below, however the accept race resolves.
    let mut held = TcpStream::connect(proxy.address()).await.unwrap();
    let _ = held
        .write_all(b"CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n")
        .await;
    proxy.stop_if_scope_ended(&scope("run"));
    assert!(proxy.state.stopped.load(Ordering::Acquire));
    assert_post_stop_connect_is_not_served(&proxy, server.address).await;
    // The held pre-stop connection is closed by the stop instead of being
    // served: the read ends (EOF or reset) rather than hanging, and the
    // vault saw nothing from it.
    let mut buf = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), held.read_to_end(&mut buf)).await;
    assert!(
        closed.is_ok(),
        "stop must close the accepted-but-unserved connection"
    );
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["error"],
        true
    );
    assert_eq!(vault.calls.load(Ordering::SeqCst), 1);
    assert!(proxy.state.patterns.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_rotating_material_keeps_both_matchers_for_overlapping_responses() {
    let server = Server::new("overlap").await;
    let vault = Vault::new();
    vault.rotate.store(true, Ordering::SeqCst);
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(true);
    let responses = client.command(json!({"op":"overlap"}));
    for field in ["a", "b"] {
        let body = responses[field]
            .as_str()
            .expect("both h2 streams completed");
        assert_eq!(body.matches(MARKER).count(), 11, "{body}");
        assert!(!body.contains(std::str::from_utf8(K1).unwrap()));
        assert!(!body.contains(std::str::from_utf8(K2).unwrap()));
    }
    assert_eq!(vault.calls.load(Ordering::SeqCst), 2);
    assert_eq!(proxy.state.patterns.lock().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_peer_identity_rejects_outside_process_other_session_and_setsid_descendant() {
    let server = Server::new("plain").await;
    let vault = Vault::new();
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut command = Client::spawn(&proxy, &server);
    proxy.arm_command(command.process.id()).unwrap();
    // An outside process has the right second factor but not the command SID.
    let mut socket = TcpStream::connect(proxy.address()).await.unwrap();
    let credentials = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:", proxy.state.token.as_str()));
    socket.write_all(format!("CONNECT localhost:{} HTTP/1.1\r\nHost: localhost:{}\r\nProxy-Authorization: Basic {credentials}\r\n\r\n",server.address.port(),server.address.port()).as_bytes()).await.unwrap_or(());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), socket.read_u8())
            .await
            .unwrap()
            .is_err()
    );
    let mut other_session = Client::spawn(&proxy, &server);
    assert_eq!(
        other_session.command(json!({"op":"connect"}))["error"],
        true
    );
    assert_eq!(command.command(json!({"op":"escaped"}))["error"], true);
    command.connect(false);
    assert_eq!(
        command.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        200
    );
    assert_eq!(vault.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_process_tree_end_stops_listener_and_discards_matchers() {
    let server = Server::new("plain").await;
    let vault = Vault::new();
    let mut proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        200
    );
    client.process.kill().unwrap();
    client.process.wait().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !proxy.state.stopped.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    proxy.process_tree_ended();
    assert_post_stop_connect_is_not_served(&proxy, server.address).await;
    assert!(proxy.state.patterns.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_query_and_json_escape_material_without_partial_header_writes() {
    let server = Server::new("echo-request").await;
    let vault = Vault::new();
    vault.special.store(true, Ordering::SeqCst);
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[
                ("query", Location::Query("key".into())),
                ("json", Location::Json("/key".into())),
            ],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let response = client.request(
        "/approved?key=%7B%7Bck-secret%3Aquery%7D%7D",
        json!({"content-type":"application/json"}),
        r#"{"key":"{{ck-secret:json}}"}"#,
    );
    assert_eq!(response["status"], 200, "{response}");
    let seen = server.seen.lock()[0].clone();
    assert_eq!(
        seen["target"],
        "https://localhost:".to_owned()
            + &server.address.port().to_string()
            + "/approved?key=alpha%26percent%25quote%22slash%5C%0D%0Akey"
    );
    let body: Value = serde_json::from_str(seen["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["key"], std::str::from_utf8(SPECIAL).unwrap());
    assert!(!response
        .to_string()
        .contains("alpha%26percent%25quote%22slash%5C%0D%0Akey"));
    drop(client);
    drop(proxy);
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        403
    );
    assert_eq!(server.seen.lock().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_expired_grant_refuses_without_material_and_tunnel_closes_on_stop() {
    let server = Server::new("echo-request").await;
    let vault = Vault::new();
    let mut cfg = config(
        vault.clone(),
        server.address.port(),
        &[("test", Location::Bearer)],
    );
    cfg.grants[0].expires_at_ms = 0;
    let mut proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        403
    );
    assert_eq!(vault.calls.load(Ordering::SeqCst), 0);
    assert!(server.seen.lock().is_empty());
    assert_eq!(
        client.command(json!({"op":"connect","authority":server.address.to_string()}))["ok"],
        true
    );
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        200
    );
    proxy.process_tree_ended();
    assert_post_stop_connect_is_not_served(&proxy, server.address).await;
    assert_eq!(client.request("/approved", json!({}), "")["error"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_http1_upstream_receives_all_locations_and_rewritten_length() {
    let server = Server::new("echo-request-h1").await;
    let vault = Vault::new();
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[
                ("test", Location::Bearer),
                ("query", Location::Query("q".into())),
                ("header", Location::Header("x-key".into())),
                ("json", Location::Json("/k".into())),
            ],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let response = client.request("/approved?q=%7B%7Bck-secret%3Aquery%7D%7D", json!({"authorization":format!("Bearer {MARKER}"),"x-key":"{{ck-secret:header}}","content-type":"application/json"}), r#"{"k":"{{ck-secret:json}}"}"#);
    assert_eq!(response["status"], 200, "{response}");
    let seen = server.seen.lock()[0].clone();
    assert_eq!(seen["target"], "/approved?q=test-key-12345678");
    assert_eq!(seen["headers"]["authorization"], "Bearer test-key-12345678");
    assert_eq!(seen["headers"]["x-key"], "test-key-12345678");
    assert_eq!(seen["body"], r#"{"k":"test-key-12345678"}"#);
    assert_eq!(seen["headers"]["content-length"], "25");
}

fn ws_message(mut bytes: &[u8]) -> Vec<u8> {
    let mut message = Vec::new();
    let mut first = true;
    loop {
        assert!(bytes.len() >= 2);
        let flags = bytes[0];
        let size = bytes[1];
        bytes = &bytes[2..];
        assert_eq!(flags & 0x70, 0);
        assert_eq!(size & 0x80, 0);
        assert_eq!(flags & 0xf, if first { 1 } else { 0 });
        first = false;
        let length = match size {
            126 => {
                let length = u16::from_be_bytes(bytes[..2].try_into().unwrap()) as usize;
                bytes = &bytes[2..];
                length
            }
            127 => {
                let length = u64::from_be_bytes(bytes[..8].try_into().unwrap()) as usize;
                bytes = &bytes[8..];
                length
            }
            size => size as usize,
        };
        message.extend_from_slice(&bytes[..length]);
        bytes = &bytes[length..];
        if flags & 0x80 != 0 {
            assert!(bytes.is_empty());
            return message;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_requires_armed_session_token_and_canonical_connect_authority() {
    let server = Server::new("plain").await;
    let vault = Vault::new();
    let proxy = SecretProxy::start_with_tls(
        config(
            vault.clone(),
            server.address.port(),
            &[("test", Location::Bearer)],
        ),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    assert_eq!(client.command(json!({"op":"connect"}))["error"], true);
    proxy.arm_command(client.process.id()).unwrap();
    assert_eq!(
        client.command(json!({"op":"connect","wrong_token":true}))["error"],
        true
    );
    assert_eq!(
        client.command(json!({"op":"connect","connect_host":"other.invalid"}))["error"],
        true
    );
    client.connect(false);
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        200
    );
    assert_eq!(vault.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn proxy_only_accepts_grants_for_the_exact_launch_scan() {
    let vault = Vault::new();
    let make = || config(vault.clone(), 443, &[("test", Location::Bearer)]);
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let mut cfg = make();
    cfg.scanned_labels.clear();
    assert_eq!(
        SecretProxy::start_with_tls(cfg, tls.clone()).err(),
        Some(ProxyError::InvalidRules)
    );
    let mut cfg = make();
    cfg.scanned_labels.push("file-label".into());
    assert_eq!(
        SecretProxy::start_with_tls(cfg, tls.clone()).err(),
        Some(ProxyError::InvalidRules)
    );
    let mut cfg = make();
    cfg.grants[0].rule.location = Location::Header("Accept-Encoding".into());
    assert_eq!(
        SecretProxy::start_with_tls(cfg, tls).err(),
        Some(ProxyError::InvalidRules)
    );
}

/// A stopped proxy's listener is closed before `stop()` returns, so a fresh
/// connect is normally refused. It can still succeed when the kernel finished
/// the handshake before the close, or when another test has since bound the
/// recycled port. Such a socket must never be served by this proxy: no
/// success response for an authenticated CONNECT.
async fn assert_post_stop_connect_is_not_served(
    proxy: &SecretProxy,
    authority: std::net::SocketAddr,
) {
    if let Ok(mut stray) = TcpStream::connect(proxy.address()).await {
        let url = proxy.proxy_url();
        let token = url
            .strip_prefix("http://")
            .and_then(|rest| rest.split_once(":@"))
            .map(|(token, _)| token.to_owned());
        if let Some(token) = token {
            let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
            let _ = stray
                .write_all(
                    format!(
                        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\
                         Proxy-Authorization: Basic {credentials}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await;
        }
        let mut reply = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), stray.read_to_end(&mut reply)).await;
        assert!(
            !reply.starts_with(b"HTTP/1.1 200"),
            "post-stop connection was served: {}",
            String::from_utf8_lossy(&reply)
        );
    }
}
