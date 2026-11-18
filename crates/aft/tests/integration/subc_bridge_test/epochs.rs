#[allow(unused_imports)]
use super::*;

#[test]
fn fake_daemon_mints_epochs_and_drops_stale_frames_in_both_directions() {
    let mut consumer = FakeDaemonDemux::default();
    let mut provider = FakeDaemonDemux::default();
    let consumer_epoch_1 = consumer.grant(4);
    let provider_epoch_1 = provider.grant(9);

    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        4,
        consumer_epoch_1,
        71,
        br#"{}"#.to_vec(),
    )
    .unwrap();
    let provider_request = consumer
        .rewrite(&request, 9, provider_epoch_1)
        .expect("current client request");
    assert_eq!(
        (
            provider_request.header.channel,
            provider_request.header.epoch
        ),
        (9, provider_epoch_1)
    );

    consumer.close(4, consumer_epoch_1);
    provider.close(9, provider_epoch_1);
    let consumer_epoch_2 = consumer.grant(4);
    let provider_epoch_2 = provider.grant(9);
    assert_eq!((consumer_epoch_2, provider_epoch_2), (2, 2));
    assert!(consumer.rewrite(&request, 9, provider_epoch_2).is_none());

    let stale_response = Frame::build(
        FrameType::Response,
        control_flags(),
        9,
        provider_epoch_1,
        71,
        br#"{}"#.to_vec(),
    )
    .unwrap();
    assert!(provider
        .rewrite(&stale_response, 4, consumer_epoch_2)
        .is_none());
    assert_eq!(consumer.installed.get(&4), Some(&consumer_epoch_2));
    assert_eq!(provider.installed.get(&9), Some(&provider_epoch_2));
}
