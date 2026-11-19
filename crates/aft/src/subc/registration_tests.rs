use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;
use subc_os::registration_channel::{ChildDescriptorSlots, DAEMON_PID_ENV, REGISTRATION_FD_ENV};
use subc_protocol::claim::{Claim, ClaimPresentReply, ClaimPresentRequest, ClaimPurpose};
use subc_protocol::{Flags, Frame, Priority};
use subc_transport::connection_file::{ConnectionInfo, Endpoint};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::net::{TcpListener, TcpStream};

use super::super::{readiness::PendingControlReplies, wire::WriterFrame};

const KEY: [u8; subc_transport::KEY_LEN] = [0x24; subc_transport::KEY_LEN];
const DAEMON: [u8; subc_transport::DAEMON_ID_LEN] = [0x42; subc_transport::DAEMON_ID_LEN];
const VERSION: &str = "registration-test";
const CHILD: &str = "AFT_TEST_REGISTRATION_CONSUMER_CHILD";

#[test]
fn registration_consumers_wait_bind_and_fail_closed() {
    if std::env::var_os(CHILD).is_some() {
        let adopted = AdoptedRegistration::adopt().unwrap().unwrap();
        adopted.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(25), consumer_cases())
                .await
                .unwrap();
        });
        return;
    }

    // The SDK's adoption and minter are once per process, so install them
    // in a fresh process rather than contaminating unrelated consumer tests.
    let (_peer, client) = std::os::unix::net::UnixStream::pair().unwrap();
    let client = std::fs::File::from(std::os::fd::OwnedFd::from(client));
    let inode = client.metadata().unwrap().ino();
    let mut slots = ChildDescriptorSlots::default();
    slots.channel(client.into()).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "subc::registration::tests::registration_consumers_wait_bind_and_fail_closed",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env(subc_protocol::SUBC_MODULE_ID_ENV, "aft")
        .env(DAEMON_PID_ENV, std::process::id().to_string())
        .env(REGISTRATION_FD_ENV, format!("4:{inode}"));
    slots.install(&mut command);
    let child = command.output().unwrap();
    assert!(
        child.status.success(),
        "child: {:?}\n{}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(
        String::from_utf8_lossy(&child.stdout).contains("1 passed"),
        "child did not run the test"
    );
}

async fn authenticate_consumer(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().await.unwrap();
    authenticate_server(&mut stream, &KEY, &DAEMON, VERSION, Duration::from_secs(2))
        .await
        .unwrap();
    stream
}

fn claim_error(result: Result<SubcConsumer, ConsumerError>) -> ClaimError {
    let Err(ConsumerError::Connect { source, .. }) = result else {
        panic!("expected a failed claim binding");
    };
    *source
        .into_inner()
        .unwrap()
        .downcast::<ClaimError>()
        .unwrap()
}

async fn mint_request(rx: &mut tokio::sync::mpsc::Receiver<WriterFrame>) -> u64 {
    let request = rx.recv().await.unwrap();
    let frame = request.frame();
    assert_eq!(frame.header.ty, FrameType::Request);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.epoch, 0);
    assert_eq!(
        serde_json::from_slice::<ClaimMintRequest>(&frame.body)
            .unwrap()
            .purpose,
        ClaimPurpose::SelfConnection
    );
    frame.header.corr
}

fn reply(ty: FrameType, corr: u64, body: impl serde::Serialize) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

async fn consumer_cases() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = fixture.path().join("subc-connection.json");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let info = ConnectionInfo {
        schema: connection_file::SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: KEY.to_vec(),
        daemon_id: DAEMON,
        pid: std::process::id(),
        daemon_ver: VERSION.into(),
    };
    connection_file::write_atomic(&path, &info).unwrap();

    assert!(!identity_ready());
    let open = || connect_consumer(&path, ConsumerOptions::default());
    tokio::select! {
        result = open() => assert!(matches!(claim_error(result), ClaimError::Unavailable)),
        _ = listener.accept() => panic!("consumer dialed before the minter was installed"),
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let pending = PendingControlReplies::default();
    let control = Arc::new(DaemonControl::new(
        tx,
        Arc::new(DispatchPathMetrics::new()),
        subc_protocol::PROTOCOL_VERSION,
        vec![],
        vec![],
        pending.clone(),
        2,
    ));
    let mut opened = Box::pin(super::super::consent::connect_for_registration_test(&path));
    tokio::select! {
        _ = &mut opened => panic!("consumer did not wait for the minter"),
        _ = listener.accept() => panic!("consumer dialed before the minter was installed"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {},
    }
    let lifetime = install_minter(&control).unwrap();
    assert!(identity_ready());
    let (consumer, ()) = tokio::join!(opened, async {
        let mut peer = authenticate_consumer(&listener).await;
        let first = mint_request(&mut rx).await;
        let at = tokio::time::Instant::now();
        assert!(pending.resolve(reply(FrameType::Error, first, serde_json::json!({
            "code":error_codes::CLAIM_RATE_LIMITED,"message":"rate limited","detail":{"retry_after_ms":50}
        }))));
        let next = mint_request(&mut rx).await;
        assert_ne!(first, next);
        assert!(at.elapsed() >= Duration::from_millis(50));
        let claim = Claim::new("fixture-claim");
        assert!(pending.resolve(reply(
            FrameType::Response,
            next,
            ClaimMintReply::new(claim.clone(), 0)
        )));
        let present = read_frame(&mut peer).await.unwrap().unwrap();
        assert_eq!(present.header.channel, 0);
        assert_eq!(
            serde_json::from_slice::<ClaimPresentRequest>(&present.body)
                .unwrap()
                .claim,
            claim
        );
        write_frame(
            &mut peer,
            &reply(
                FrameType::Response,
                present.header.corr,
                ClaimPresentReply::new(),
            ),
        )
        .await
        .unwrap();
    });
    drop(consumer.unwrap());

    let (refused, ()) = tokio::join!(open(), async {
        let mut peer = authenticate_consumer(&listener).await;
        let corr = mint_request(&mut rx).await;
        assert!(pending.resolve(reply(
            FrameType::Error,
            corr,
            ErrorBody::new("claim_refused", "fixture refused")
        )));
        assert!(
            read_frame(&mut peer).await.unwrap().is_none(),
            "refused socket must close without sending a claim or route"
        );
    });
    assert!(
        matches!(claim_error(refused), ClaimError::Refused { code } if code == "claim_refused")
    );

    let (closed, ()) = tokio::join!(open(), async {
        let mut peer = authenticate_consumer(&listener).await;
        let _ = mint_request(&mut rx).await;
        drop(lifetime);
        assert!(read_frame(&mut peer).await.unwrap().is_none());
    });
    assert!(matches!(claim_error(closed), ClaimError::Unavailable));
    assert_eq!(pending.len(), 0);

    let (later, ()) = tokio::join!(open(), async {
        let mut peer = authenticate_consumer(&listener).await;
        tokio::select! {
            _ = rx.recv() => panic!("closed registration sent another mint"),
            reply = read_frame(&mut peer) => assert!(reply.unwrap().is_none()),
        }
    });
    assert!(matches!(claim_error(later), ClaimError::Unavailable));
}
