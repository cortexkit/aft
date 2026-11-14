pub use aft_base::url_fetch::sensitive::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::ops::{Deref, DerefMut};
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::{Zeroize, Zeroizing};

#[cfg(test)]
mod tests {
    use super::super::*;
    use std::io::Cursor;
    const KEY: &[u8] = b"echo-fetch-key-0123456789";

    struct FailsAfterBody {
        body: Cursor<Vec<u8>>,
        pause: Duration,
    }
    impl Read for FailsAfterBody {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.body.position() < self.body.get_ref().len() as u64 {
                return self.body.read(output);
            }
            thread::sleep(self.pause);
            Err(io::Error::other(
                "echoed read failure echo-fetch-key-0123456789",
            ))
        }
    }

    #[test]
    fn secret_body_success_wipes_each_echo_chunk_and_old_accumulator_before_growth() {
        let (trace, _scope) = super::testing::track(KEY);
        let input = KEY.repeat(8000);
        let (body, cut) = read_response_body_with(
            Cursor::new(input.clone()),
            "https://api.test/doc",
            &FetchDeadline::for_current_call(),
            &str::to_owned,
        )
        .unwrap();
        assert!(!cut);
        assert_eq!(&*body, &input);
        drop(body);
        let wipes = trace.wait_for(4);
        assert!(
            wipes.len() >= 4,
            "chunk, scratch and accumulator owners all wipe"
        );
        assert!(
            wipes.iter().any(|wipe| wipe.growth),
            "old populated allocations must wipe before growth frees them"
        );
        assert!(wipes.iter().all(|wipe| wipe.zeroed));
    }

    #[test]
    fn secret_body_read_error_wipes_echo_chunks_accumulator_and_formatted_error() {
        let (trace, _scope) = super::testing::track(KEY);
        let mut held = HeldKeys::default();
        held.hold("test", KEY).unwrap();
        let input = FailsAfterBody {
            body: Cursor::new(KEY.repeat(4000)),
            pause: Duration::ZERO,
        };
        let error = read_response_body_with(
            input,
            "https://api.test/doc",
            &FetchDeadline::for_current_call(),
            &|detail| held.sanitize_detail(detail),
        )
        .unwrap_err();
        assert!(error.to_string().contains("{{ck-secret:test}}"));
        assert!(!error
            .to_string()
            .contains(std::str::from_utf8(KEY).unwrap()));
        let wipes = trace.wait_for(4);
        assert!(wipes.len() >= 4);
        assert!(wipes.iter().any(|wipe| wipe.growth));
        assert!(
            wipes.iter().any(|wipe| wipe.capacity < 1024),
            "the formatted peer error must have a wiping owner too"
        );
        assert!(wipes.iter().all(|wipe| wipe.zeroed));
    }

    #[test]
    fn secret_body_deadline_wipes_the_accumulator_and_queued_echoes_on_exit() {
        let (trace, _scope) = super::testing::track(KEY);
        let input = FailsAfterBody {
            body: Cursor::new(KEY.repeat(4000)),
            pause: Duration::from_millis(100),
        };
        let mut deadline = FetchDeadline::for_current_call();
        deadline.at = Instant::now() + Duration::from_millis(30);
        assert!(
            read_response_body_with(input, "https://api.test/doc", &deadline, &str::to_owned)
                .is_err()
        );
        let wipes = trace.wait_for(5);
        assert!(wipes.len() >= 5);
        assert!(wipes.iter().all(|wipe| wipe.zeroed));
    }

    struct FailingDecoder {
        body: Vec<u8>,
        cursor: usize,
    }
    impl tokio::io::AsyncRead for FailingDecoder {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            output: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            if self.cursor == self.body.len() {
                return std::task::Poll::Ready(Err(io::Error::other("injected decoder failure")));
            }
            let count = output.remaining().min(self.body.len() - self.cursor);
            output.put_slice(&self.body[self.cursor..self.cursor + count]);
            self.cursor += count;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn secret_decoder_error_wipes_scratch_output_and_old_growth_allocations() {
        let (trace, _scope) = super::testing::track(KEY);
        let reader = FailingDecoder {
            body: KEY.repeat(4000),
            cursor: 0,
        };
        assert!(drain_decoder(Box::pin(reader), false).is_err());
        let wipes = trace.wait_for(3);
        assert!(wipes.len() >= 3);
        assert!(wipes.iter().any(|wipe| wipe.growth));
        assert!(wipes.iter().all(|wipe| wipe.zeroed));
    }

    #[test]
    fn secret_outbound_serialization_is_presized_owned_and_wiped_without_url_mutation() {
        let (trace, _scope) = super::testing::track(KEY);
        let current =
            Url::parse("https://api.test:443/old?key={{ck-secret:test}}#never-sent").unwrap();
        let query = "key=echo-fetch-key-0123456789";
        let expected = format!("https://api.test/normalized?{query}");
        let outbound = secret_outbound_url(&current, "/normalized", query).unwrap();
        assert_eq!(outbound.as_str(), expected);
        assert_eq!(format!("{outbound:?}"), "ProtectedOutboundUrl");
        drop(outbound);
        let wipes = trace.wait_for(1);
        assert_eq!(
            wipes.len(),
            1,
            "one protected whole serialization, no clone/set_query intermediates"
        );
        assert!(!wipes[0].growth);
        assert_eq!(wipes[0].capacity, expected.len());
        assert!(wipes[0].zeroed);
    }

    #[test]
    fn secret_outbound_buffer_remains_zeroizing_until_reqwest_takes_its_url() {
        let (trace, _scope) = super::testing::track(KEY);
        let current = Url::parse("https://api.test/old?key={{ck-secret:test}}").unwrap();
        let outbound =
            secret_outbound_url(&current, "/normalized", "key=echo-fetch-key-0123456789").unwrap();
        assert!(trace.wait_for(0).is_empty(), "the protected serialization must stay owned until handover, not be replaced by an ordinary Url string");
        let request_url = Url::parse(outbound.as_str()).unwrap();
        let client = Client::builder().build().unwrap();
        let request = client.get(request_url).build().unwrap();
        assert_eq!(request.url().query(), Some("key=echo-fetch-key-0123456789"));
        drop(outbound);
        let wipes = trace.wait_for(1);
        assert_eq!(wipes.len(), 1);
        assert!(wipes[0].zeroed && !wipes[0].growth);
    }

    #[test]
    fn sanitize_detail_masks_raw_and_percent_encoded_key_spellings() {
        let mut held = HeldKeys::default();
        held.hold("test", KEY).unwrap();
        let raw = std::str::from_utf8(KEY).unwrap();
        for spelling in [
            raw.to_owned(),
            KEY.iter()
                .map(|byte| format!("%{byte:02X}"))
                .collect::<String>(),
            KEY.iter()
                .map(|byte| format!("%{byte:02x}"))
                .collect::<String>(),
            format!("%65{}", &raw[1..]),
        ] {
            let clean =
                held.sanitize_detail(&format!("request https://api.test/?key={spelling} failed"));
            assert!(!clean.contains(&spelling));
            assert!(!clean.contains(raw));
            assert!(
                clean.is_empty() || clean.contains("{{ck-secret:test}}"),
                "an encoded spelling is masked or dropped, never copied through"
            );
        }
    }
}
