//! Supervised peers for sending Basal's captured bytes through a real daemon.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, value::RawValue, Value};
use subc_protocol::manifest::ModuleManifest;
use subc_protocol::scope::{
    ScopeAttributes, ScopeCarrier, ScopeKind, ScopeParent, ScopeRecord, ScopeRecordOutcome,
};
use subc_protocol::{Flags, Frame, FrameType, ModuleHelloBody, Principal, Priority};
use subc_transport::{authenticate_client, read_frame, write_frame};
use tokio::net::TcpStream;

// Basal capture: .cortexkit/internal-docs/contract-ref/basal-flow-head-read-v1/request.json
// SHA-256: 5f24c46c7b95b410c6f7da90646af22868643c6bc6c0fb34dd5491fb5dbf41d2
const CAPTURE: &[u8] = include_bytes!("../fixtures/basal-flow-head-read-v1/request.json");

#[derive(Deserialize)]
struct Capture<'a> {
    #[serde(borrow)]
    route_open: &'a RawValue,
    #[serde(borrow)]
    request: &'a RawValue,
}

fn reserved(module_id: &str) -> Principal {
    Principal::Reserved {
        module_id: module_id.into(),
    }
}

pub(super) async fn run() {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let fixture = home.parent().unwrap();
    match std::env::var("SUBC_MODULE_ID").unwrap().as_str() {
        "prefrontal" => register_scopes(fixture).await,
        "basal" => read_flows(fixture).await,
        module => panic!("unexpected Basal flow fixture module: {module}"),
    }
}

async fn register_scopes(fixture: &Path) {
    let (handle, serve) = subc_client_rs::serve_from_env_with_handle(
        ModuleManifest::builder("prefrontal", "0.0.0").build(),
        super::ScopeOwner,
    )
    .await
    .unwrap();
    let serving = tokio::spawn(serve);
    let workdir = fixture.join("project").to_string_lossy().into_owned();
    let basal = || ScopeCarrier::new(reserved("basal")).with_targets(Some(vec!["aft".into()]));
    let head = |scope_ref, epoch| {
        ScopeRecord::new(scope_ref, epoch, ScopeKind::Head)
            .with_attributes(ScopeAttributes::new().with_agent_id(Some("fixture-agent".into())))
            .with_workdir(Some(workdir.clone()))
    };
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 120_000;
    let scopes = vec![
        head("head/fixture", 37).with_carriers(vec![ScopeCarrier::new(Principal::Direct), basal()]),
        head("head/other", 38).with_carriers(vec![basal()]),
        ScopeRecord::new("flow-read/fixture", 1, ScopeKind::Ephemeral)
            .with_workdir(Some(workdir.clone()))
            .with_parent(Some(ScopeParent::new(
                reserved("prefrontal"),
                "head/fixture",
                37,
            )))
            .with_expires_at_ms(Some(expires))
            .with_carriers(vec![basal()]),
    ];
    let reply = handle.scope_sync(1, scopes).await.unwrap();
    assert_eq!(reply.results.len(), 3, "{reply:?}");
    assert!(
        reply
            .results
            .iter()
            .all(|row| row.outcome != ScopeRecordOutcome::Refused),
        "{reply:?}"
    );
    std::fs::write(fixture.join("scope-owner.ready"), b"ready").unwrap();
    serving.await.unwrap().unwrap();
}

async fn read_flows(fixture: &Path) {
    use sha2::{Digest, Sha256};
    use subc_protocol::claim::{
        ClaimMintReply, ClaimMintRequest, ClaimPresentRequest, ClaimPurpose,
    };

    assert_eq!(
        format!("{:x}", Sha256::digest(CAPTURE)),
        "5f24c46c7b95b410c6f7da90646af22868643c6bc6c0fb34dd5491fb5dbf41d2"
    );
    let capture: Capture<'_> = serde_json::from_slice(CAPTURE).unwrap();
    let info =
        subc_transport::read_for_client(&fixture.join("runtime/subc-connection.json")).unwrap();
    let endpoint = &info.endpoints[0];
    let connect = || async {
        let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .unwrap();
        authenticate_client(&mut stream, &info, Duration::from_secs(5))
            .await
            .unwrap();
        stream
    };
    let mut registration = subc_client_rs::registration::adopt_channel()
        .unwrap()
        .expect("supervised Basal registration channel");
    authenticate_client(&mut registration, &info, Duration::from_secs(5))
        .await
        .unwrap();
    let hello = ModuleHelloBody {
        manifest: ModuleManifest::builder("basal", "0.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    let flags = Flags::new(false, Priority::Interactive, false);
    let hello = Frame::build(
        FrameType::Hello,
        flags,
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    write_frame(&mut registration, &hello).await.unwrap();
    let ack = read_frame(&mut registration).await.unwrap().unwrap();
    assert_eq!(ack.header.ty, FrameType::HelloAck, "{ack:?}");

    // The daemon accepts route.open on a consumer socket, not the module's
    // registration socket. Bind it to Basal before sending the captured body.
    let minted = exchange(
        &mut registration,
        0,
        0,
        2,
        &serde_json::to_vec(&ClaimMintRequest::new(ClaimPurpose::SelfConnection)).unwrap(),
    )
    .await;
    assert_eq!(
        minted.header.ty,
        FrameType::Response,
        "claim.mint refused: {minted:?}"
    );
    let ClaimMintReply { claim, .. } = serde_json::from_slice(&minted.body).unwrap();
    let mut stream = connect().await;
    let presented = exchange(
        &mut stream,
        0,
        0,
        1,
        &serde_json::to_vec(&ClaimPresentRequest::new(claim)).unwrap(),
    )
    .await;
    assert_eq!(
        presented.header.ty,
        FrameType::Response,
        "claim.present refused: {presented:?}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    while !fixture.join("basal-flow-read.start").exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "head route did not open"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let route = capture.route_open.get().as_bytes();
    let script = capture.request.get().as_bytes();
    // Basal captured the script request; the manifest request changes only path.
    let manifest = capture
        .request
        .get()
        .replace("flows/sample.js", "flows/sample.json");
    let with_scope = |scope_ref, epoch| {
        let mut body: Value = serde_json::from_slice(route).unwrap();
        body["scope"]["ref"] = json!(scope_ref);
        body["scope"]["scope_epoch"] = json!(epoch);
        serde_json::to_vec(&body).unwrap()
    };
    let mut outcomes = serde_json::Map::new();
    for (i, (case, open, request)) in [
        ("head-script", route, script),
        ("head-manifest", route, manifest.as_bytes()),
        ("other-head", &with_scope("head/other", 38), script),
        ("flow-read", &with_scope("flow-read/fixture", 1), script),
    ]
    .into_iter()
    .enumerate()
    {
        let corr = 10 + i as u64 * 2;
        let opened = exchange(&mut stream, 0, 0, corr, open).await;
        let handle: Value = serde_json::from_slice(&opened.body).unwrap();
        if opened.header.ty != FrameType::Response {
            outcomes.insert(case.into(), json!({"stage":"route.open", "body":handle}));
            break;
        }
        let channel = u16::try_from(handle["route_channel"].as_u64().unwrap()).unwrap();
        let epoch = u32::try_from(handle["route_epoch"].as_u64().unwrap()).unwrap();
        let reply = exchange(&mut stream, channel, epoch, corr + 1, request).await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        outcomes.insert(case.into(), json!({"stage":"file.read_exact", "success":reply.header.ty == FrameType::Response, "body":body}));
    }
    let result = fixture.join("basal-flow-read.result");
    let temp = result.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_vec(&outcomes).unwrap()).unwrap();
    std::fs::rename(temp, result).unwrap();
}

async fn exchange(
    stream: &mut impl subc_client_rs::ModuleStream,
    channel: u16,
    epoch: u32,
    corr: u64,
    body: &[u8],
) -> Frame {
    use subc_protocol::session::{HealthReport, ModuleControlRequest, ModuleControlResponse};

    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        body.to_vec(),
    )
    .unwrap();
    write_frame(stream, &request).await.unwrap();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), read_frame(stream))
            .await
            .unwrap()
            .unwrap()
            .expect("Basal connection closed during request");
        let (ty, body) = match frame.header.ty {
            FrameType::Ping => (FrameType::Pong, vec![]),
            FrameType::Request if frame.header.channel == 0 => {
                let ModuleControlRequest::HealthCheck {} =
                    serde_json::from_slice(&frame.body).unwrap()
                else {
                    panic!("unexpected module control request: {frame:?}");
                };
                (
                    FrameType::Response,
                    serde_json::to_vec(&ModuleControlResponse::from(HealthReport::ok())).unwrap(),
                )
            }
            FrameType::Push => continue,
            FrameType::Response | FrameType::Error => {
                assert_eq!(
                    (frame.header.channel, frame.header.corr),
                    (channel, corr),
                    "{frame:?}"
                );
                return frame;
            }
            _ => panic!("unexpected frame during Basal request: {frame:?}"),
        };
        let reply = Frame::build_with_version(
            frame.header.ver,
            ty,
            frame.header.flags,
            0,
            0,
            frame.header.corr,
            body,
        )
        .unwrap();
        write_frame(stream, &reply).await.unwrap();
    }
}
