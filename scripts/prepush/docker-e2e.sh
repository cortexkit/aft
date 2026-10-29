#!/usr/bin/env bash
set -euo pipefail
docker version
mkdir -p tests/docker/fixtures/npm-packs e2e-artifacts/linux
cp "${CARGO_TARGET_DIR:-target}/stage/aft" tests/docker/fixtures/aft-linux-x64
chmod +x tests/docker/fixtures/aft-linux-x64
bash tests/docker/check-bind-mount-root.sh tests/docker/fixtures/aft-linux-x64
npm pack ./packages/aft-bridge --pack-destination tests/docker/fixtures/npm-packs
npm pack ./packages/opencode-plugin --pack-destination tests/docker/fixtures/npm-packs
docker build --platform linux/amd64 -t aft-e2e-linux-x64 -f tests/docker/Dockerfile.linux-x64 .
docker run --rm --platform linux/amd64 \
  --volume "$PWD/e2e-artifacts/linux:/root/.local/share/cortexkit/aft/logs" aft-e2e-linux-x64
