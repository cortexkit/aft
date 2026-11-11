//! Fetch-owned response and formatting buffers wipe before every allocation is freed.
use std::ops::{Deref, DerefMut};
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Default)]
pub(super) struct Factory {
    #[cfg(test)]
    trace: Option<std::sync::Arc<testing::Trace>>,
}
impl Factory {
    pub(super) fn current() -> Self {
        Self {
            #[cfg(test)]
            trace: testing::CURRENT.with(|slot| slot.borrow().clone()),
        }
    }
    pub(super) fn with_capacity(&self, capacity: usize) -> Bytes {
        Bytes {
            data: Zeroizing::new(Vec::with_capacity(capacity)),
            factory: self.clone(),
        }
    }
    pub(super) fn copy(&self, bytes: &[u8]) -> Bytes {
        let mut owned = self.with_capacity(bytes.len());
        owned.extend_from_slice(bytes);
        owned
    }
    pub(super) fn zeroed(&self, length: usize) -> Bytes {
        let mut owned = self.with_capacity(length);
        owned.data.resize(length, 0);
        owned
    }
    pub(super) fn format(&self, args: std::fmt::Arguments<'_>) -> Bytes {
        use std::fmt::Write;
        let mut owned = self.with_capacity(0);
        owned
            .write_fmt(args)
            .expect("formatting into owned memory cannot fail");
        owned
    }
    fn wipe(&self, data: &mut Vec<u8>, growth: bool) {
        #[cfg(test)]
        let contained_key = self
            .trace
            .as_ref()
            .is_some_and(|trace| trace.contains(data));
        data.as_mut_slice().zeroize();
        #[cfg(test)]
        if contained_key {
            self.trace.as_ref().unwrap().record(data, growth);
        }
        #[cfg(not(test))]
        let _ = growth;
    }
}

pub(super) struct Bytes {
    data: Zeroizing<Vec<u8>>,
    factory: Factory,
}
impl Bytes {
    pub(super) fn extend_from_slice(&mut self, bytes: &[u8]) {
        let required = self
            .data
            .len()
            .checked_add(bytes.len())
            .expect("buffer length overflow");
        if required > self.data.capacity() {
            // Vec::reserve may free a populated allocation without wiping it.
            // Copy into a new owner, then wipe the old allocation before drop.
            let capacity = required.max(self.data.capacity().saturating_mul(2));
            let mut next = Zeroizing::new(Vec::with_capacity(capacity));
            next.extend_from_slice(&self.data);
            self.factory.wipe(&mut self.data, true);
            self.data = next;
        }
        self.data.extend_from_slice(bytes);
    }
    pub(super) fn into_plain_vec(mut self) -> Vec<u8> {
        // Only the ordinary, non-secret downloader transfers plain ownership.
        std::mem::take(&mut *self.data)
    }
}
impl Drop for Bytes {
    fn drop(&mut self) {
        self.factory.wipe(&mut self.data, false);
    }
}
impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}
impl DerefMut for Bytes {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}
impl std::fmt::Write for Bytes {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.extend_from_slice(text.as_bytes());
        Ok(())
    }
}
impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtectedFetchBytes")
            .field("length", &self.len())
            .finish()
    }
}

#[cfg(test)]
pub(super) mod testing {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};
    thread_local! { pub(super) static CURRENT: std::cell::RefCell<Option<Arc<Trace>>> = const { std::cell::RefCell::new(None) }; }
    #[derive(Clone, Debug)]
    pub(in crate::url_fetch) struct Wipe {
        pub growth: bool,
        pub zeroed: bool,
        pub capacity: usize,
    }
    pub(in crate::url_fetch) struct Trace {
        needle: &'static [u8],
        records: Mutex<Vec<Wipe>>,
        changed: Condvar,
    }
    impl Trace {
        pub(super) fn contains(&self, bytes: &[u8]) -> bool {
            bytes
                .windows(self.needle.len())
                .any(|window| window == self.needle)
        }
        pub(super) fn record(&self, bytes: &Vec<u8>, growth: bool) {
            self.records.lock().unwrap().push(Wipe {
                growth,
                zeroed: bytes.iter().all(|byte| *byte == 0),
                capacity: bytes.capacity(),
            });
            self.changed.notify_all();
        }
        pub(in crate::url_fetch) fn wait_for(&self, count: usize) -> Vec<Wipe> {
            let end = Instant::now() + Duration::from_secs(2);
            let mut records = self.records.lock().unwrap();
            while records.len() < count && Instant::now() < end {
                records = self
                    .changed
                    .wait_timeout(records, end.saturating_duration_since(Instant::now()))
                    .unwrap()
                    .0;
            }
            records.clone()
        }
    }
    pub(in crate::url_fetch) struct Restore(Option<Arc<Trace>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|slot| slot.replace(self.0.take()));
        }
    }
    pub(in crate::url_fetch) fn track(needle: &'static [u8]) -> (Arc<Trace>, Restore) {
        let trace = Arc::new(Trace {
            needle,
            records: Default::default(),
            changed: Default::default(),
        });
        let restore = Restore(CURRENT.with(|slot| slot.replace(Some(trace.clone()))));
        (trace, restore)
    }
}

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
