//! Exercise the engine's real per-call socket grant with a disposable Git
//! credential helper. Neither the helper nor its reply is a production proxy.

use super::{path_arg, quote};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) const SUCCESS: &str = "GIT_CREDENTIAL_HELPER_PER_CALL_SOCKET_OK";
const RESPONSE: &str = "username=fixture\npassword=disposable-per-call-response\n";

pub(super) struct Call {
    path: PathBuf,
    server: JoinHandle<()>,
}

impl Call {
    pub(super) fn bind(temp: &Path) -> Self {
        let path = temp.join("g");
        let listener = UnixListener::bind(&path).expect("bind disposable per-call socket");
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(45);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "git did not connect to its per-call socket"
                        );
                        thread::sleep(Duration::from_millis(15));
                    }
                    Err(error) => panic!("accept per-call socket: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = String::new();
            stream.read_to_string(&mut request).unwrap();
            assert!(
                request.lines().any(|line| line == "protocol=https"),
                "{request:?}"
            );
            assert!(
                request
                    .lines()
                    .any(|line| line == "host=call-probe.invalid"),
                "{request:?}"
            );
            stream.write_all(RESPONSE.as_bytes()).unwrap();
        });
        Self { path, server }
    }

    pub(super) fn command(&self) -> String {
        let helper = quote(&format!(
            "!/usr/bin/perl -MIO::Socket::UNIX -MSocket -e {} {}",
            quote(concat!(
                "my $call = IO::Socket::UNIX->new(Type => SOCK_STREAM, Peer => shift) or die $!; ",
                "local $/; print {$call} scalar <STDIN>; ",
                "$call->flush; shutdown($call, SHUT_WR) or die $!; ",
                "print scalar <$call>;"
            )),
            path_arg(&self.path)
        ));
        format!(
            "\nreply=$(printf '%s\\n' protocol=https host=call-probe.invalid '' | \
              GIT_TERMINAL_PROMPT=0 /usr/bin/git -c credential.helper= -c credential.helper={helper} credential fill) && \
              printf '%s\\n' \"$reply\" | /usr/bin/grep -Fxq 'password=disposable-per-call-response' && \
              printf '{SUCCESS}\\n'\n"
        )
    }

    pub(super) fn finish(self) {
        self.server
            .join()
            .expect("Git credential per-call socket transaction failed");
    }
}
