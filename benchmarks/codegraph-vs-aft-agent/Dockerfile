FROM rust:1.95-bookworm

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    unzip \
    nodejs \
    npm \
    pkg-config \
    libssl-dev \
  && rm -rf /var/lib/apt/lists/*

# OpenCode currently supports Node 20+; install Node 24 to match the Docker E2E pattern.
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
  && apt-get install -y nodejs \
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
ENV PATH="/root/.bun/bin:/root/.opencode/bin:${PATH}"

ARG OPENCODE_VERSION
COPY .github/opencode-version.txt /tmp/opencode-version.txt
RUN OPENCODE_VERSION="${OPENCODE_VERSION:-$(tr -d '[:space:]' < /tmp/opencode-version.txt)}" \
  && if [ -z "$OPENCODE_VERSION" ]; then echo "OPENCODE_VERSION must be set" >&2; exit 1; fi \
  && curl -fsSL https://opencode.ai/install | bash -s -- --version "$OPENCODE_VERSION" \
  && rm /tmp/opencode-version.txt

RUN npm install -g @colbymchenry/codegraph@0.9.6

WORKDIR /workspace
COPY . /workspace

RUN bun install --frozen-lockfile
RUN cargo build --release -p agent-file-tools \
  && cp /workspace/target/release/aft /usr/local/bin/aft
RUN bun run --filter @cortexkit/aft-bridge build \
  && bun run --filter @cortexkit/aft-opencode build

# Pre-cache the published AFT plugin package, then overwrite its dist and binary
# with the local checkout so the benchmark measures this branch without requiring
# a local host install.
RUN mkdir -p /root/.cache/opencode/packages \
  && cd /root/.cache/opencode/packages \
  && npm install @cortexkit/aft-opencode@0.32.0 \
  && AFT_VER=$(node -p "require('/root/.cache/opencode/packages/node_modules/@cortexkit/aft-opencode/package.json').version") \
  && mkdir -p /root/.cache/aft/bin/v${AFT_VER} \
  && cp /workspace/target/release/aft /root/.cache/aft/bin/v${AFT_VER}/aft \
  && chmod +x /root/.cache/aft/bin/v${AFT_VER}/aft \
  && PLUGIN_DIR=$(find /root/.cache/opencode/packages -path '*/node_modules/@cortexkit/aft-opencode/dist' -type d 2>/dev/null | head -1) \
  && test -n "$PLUGIN_DIR" \
  && cp -r /workspace/packages/opencode-plugin/dist/* "$PLUGIN_DIR/"

WORKDIR /workspace/benchmarks/codegraph-vs-aft-agent
ENTRYPOINT ["bash", "scripts/run-docker.sh"]
