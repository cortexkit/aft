# Runtime image for the semantic embed RSS harness on Linux.
#
# The binary is staged in from a prebuilt artifact rather than compiled here.
# Compiling inside the image would spend wall clock producing a second copy of
# something the caller already has, and under amd64 emulation on an arm64 host
# that cost is large. `semantic-embed-rss-linux.sh` builds it once with
# tests/docker/Dockerfile.build-linux and passes the path in.
#
# Everything the harness needs is in the Python standard library, so this stays
# a slim Debian plus python3, git for the corpus repository, and procps for
# reading process state by hand when a run needs a second opinion.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 \
    git \
    procps \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The local fastembed backend loads ONNX Runtime through ORT_DYLIB_PATH. AFT
# normally downloads it into its storage directory on first use; fetching it at
# image build time instead keeps the measured run free of a one-off download,
# and ORT_DYLIB_PATH is the documented explicit override so the resolver
# short-circuits rather than searching the tree.
#
# The version and asset naming follow packages/aft-bridge/src/onnx-runtime.ts,
# which is the source of truth for what AFT expects.
ARG ORT_VERSION=1.24.4
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      amd64) ort_arch=x64 ;; \
      arm64) ort_arch=aarch64 ;; \
      *) echo "no ONNX Runtime asset for $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    asset="onnxruntime-linux-${ort_arch}-${ORT_VERSION}"; \
    curl -fsSL -o /tmp/ort.tgz \
      "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${asset}.tgz"; \
    mkdir -p /opt/onnxruntime; \
    tar -xzf /tmp/ort.tgz -C /opt/onnxruntime --strip-components=1; \
    rm /tmp/ort.tgz; \
    ls /opt/onnxruntime/lib/libonnxruntime.so*
ENV ORT_DYLIB_PATH=/opt/onnxruntime/lib/libonnxruntime.so

RUN git config --global user.email "harness@test.invalid" && \
    git config --global user.name "Harness" && \
    git config --global init.defaultBranch main

ARG AFT_BINARY=artifact/aft
COPY ${AFT_BINARY} /usr/local/bin/aft
RUN chmod +x /usr/local/bin/aft

COPY semantic-embed-rss.py /harness/semantic-embed-rss.py

WORKDIR /harness
ENTRYPOINT ["python3", "/harness/semantic-embed-rss.py"]
