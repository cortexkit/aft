use subc_protocol::manifest::ProviderRole;
use subc_protocol::session::{ModuleControlRequestFromModule, ModuleControlResponseToModule};
use subc_protocol::{Frame, FrameType};
use tokio::net::TcpStream;

use super::{control_flags, read_any_frame_timeout, send_frame};

pub(super) async fn read_ready_catalog_update(stream: &mut TcpStream) -> (u64, Vec<ProviderRole>) {
    let flip = read_any_frame_timeout(stream, "catalog.update request").await;
    assert_eq!(flip.header.ty, FrameType::Request);
    assert_eq!(flip.header.channel, 0);
    let body = serde_json::from_slice(&flip.body).expect("catalog.update body");
    match body {
        ModuleControlRequestFromModule::CatalogUpdate {
            provides,
            capabilities,
            ready,
        } => {
            assert_eq!(ready, Some(true));
            assert_eq!(capabilities, None);
            (flip.header.corr, provides)
        }
        other => panic!("expected catalog.update, got {other:?}"),
    }
}

pub(super) async fn ack_catalog_update(stream: &mut TcpStream, corr: u64) {
    send_frame(
        stream,
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&ModuleControlResponseToModule::CatalogUpdate {})
                .expect("catalog.update reply"),
        )
        .expect("catalog.update reply frame"),
    )
    .await;
}
