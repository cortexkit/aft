FROM debian:bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    procps \
    python3 \
    python3-venv \
    sqlite3 \
    jq \
    unzip \
  && rm -rf /var/lib/apt/lists/*

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
ENV FASTEMBED_CACHE_DIR="/cache/fastembed"
ENV AFT_STORAGE_DIR="/cache/aft-storage"

COPY benchmarks/settle-time/.docker/aft-linux-x64 /usr/local/bin/aft
COPY benchmarks/settle-time/run-benchmark.py /bench/run-benchmark.py
RUN chmod +x /usr/local/bin/aft /bench/run-benchmark.py

WORKDIR /bench
ENTRYPOINT ["python3", "/bench/run-benchmark.py"]
