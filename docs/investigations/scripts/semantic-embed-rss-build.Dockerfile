# Builder for the semantic embed RSS harness.
#
# tests/docker/Dockerfile.build-linux pins `FROM --platform=linux/amd64`, which
# is right for the end-to-end suites that must run the shipped x64 target but
# means a `--platform linux/arm64` build silently produces an x86-64 binary.
# This harness needs both: amd64 to match the reporter's platform, and arm64 to
# run the local ONNX backend at native speed, because emulated inference
# distorts exactly the thread and CPU behaviour the local lane is being measured
# for.
#
# Leaving the platform unpinned lets the caller choose with `--platform`.
FROM rust:1-bookworm
ARG CARGO_PROFILE=dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --profile "$CARGO_PROFILE" -p agent-file-tools
