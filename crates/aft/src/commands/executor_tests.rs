mod tests {
    #[cfg(unix)]
    use crate::executor::test_support::test_executor;
    #[cfg(unix)]
    use crate::executor::Lane;
    #[cfg(unix)]
    use aft_base::executor::tests::{ok, recv_async, test_root};
    #[cfg(unix)]
    use std::sync::atomic::Ordering;
    #[cfg(unix)]
    use std::{thread, time::Duration};
    mod stale_write;

    #[cfg(unix)]
    #[test]
    fn blocked_file_read_releases_root_reader_and_executor_slot() {
        let executor = test_executor(2, 1, 2, 1);
        let (_dir, root) = test_root("blocked-file");
        let external = tempfile::tempdir().unwrap();
        let path = external.path().join("blocked.txt");
        std::fs::write(&path, "content\n").unwrap();
        let fifo = external.path().join("consent-probe");
        crate::bounded_io::tests::fifo(&fifo);
        let probe = crate::bounded_io::tests::block_directory_probe(external.path(), &fifo);
        assert!(executor.register_actor(
            root.clone(),
            std::sync::Arc::new(crate::test_context(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config::default(),
            )),
        ));
        let req = crate::protocol::RawRequest {
            id: "blocked-read".into(),
            command: "read".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({"file": path}),
        };
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let read = executor.submit_async(
            root.clone(),
            Lane::PureRead,
            req.id.clone(),
            Box::new(move |ctx| {
                started_tx.send(()).unwrap();
                crate::bounded_io::with_budget(
                    Some(crate::bounded_io::Budget::after(Duration::from_millis(100))),
                    || crate::commands::read::handle_read(&req, ctx),
                )
            }),
        );
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let writer = executor.submit_async(
            root,
            Lane::Mutating,
            "after-blocked-read".into(),
            Box::new(|_| ok("writer")),
        );
        let (tx, rx) = crossbeam_channel::bounded(1);
        let waiter = thread::spawn(move || {
            tx.send((
                recv_async(read, "blocked read"),
                recv_async(writer, "writer"),
            ))
            .unwrap()
        });
        let before_writer = rx.recv_timeout(Duration::from_secs(1));
        crate::bounded_io::tests::release_fifo(&fifo);
        waiter.join().unwrap();
        let (response, writer_response) =
            before_writer.expect("root reader or executor slot stayed held by a blocked open");
        assert!(!response.success);
        assert_eq!(probe.fired.load(Ordering::Relaxed), 1);
        assert_eq!(response.data["code"], "read_blocked");
        assert!(writer_response.success);
    }
}
