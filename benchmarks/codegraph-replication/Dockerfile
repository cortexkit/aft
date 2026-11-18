FROM rust:1.95-bookworm

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    unzip \
    ripgrep \
    pkg-config \
    libssl-dev \
  && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://bun.sh/install | bash

ARG ORT_VERSION=1.24.4
RUN set -eux; \
  arch="$(uname -m)"; \
  case "$arch" in \
    x86_64|amd64) ort_arch="x64" ;; \
    aarch64|arm64) ort_arch="arm64" ;; \
    *) echo "Unsupported ONNX Runtime arch: $arch" >&2; exit 1 ;; \
  esac; \
  url="https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ORT_VERSION}/microsoft.ml.onnxruntime.${ORT_VERSION}.nupkg"; \
  mkdir -p /opt/onnxruntime/lib; \
  curl -fsSL "$url" -o /tmp/onnxruntime.nupkg; \
  unzip -p /tmp/onnxruntime.nupkg "runtimes/linux-${ort_arch}/native/libonnxruntime.so" > /opt/onnxruntime/lib/libonnxruntime.so; \
  rm /tmp/onnxruntime.nupkg
ENV ORT_DYLIB_PATH="/opt/onnxruntime/lib/libonnxruntime.so"
ENV LD_LIBRARY_PATH="/opt/onnxruntime/lib"
ENV PATH="/root/.bun/bin:${PATH}"

WORKDIR /workspace
COPY . /workspace

RUN bun install --frozen-lockfile
RUN cargo build --release -p agent-file-tools

WORKDIR /workspace/benchmarks/codegraph-replication
ENTRYPOINT ["bash", "scripts/run-docker.sh"]
