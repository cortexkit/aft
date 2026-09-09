# syntax=docker/dockerfile:1.7
ARG AFT_GIT_SHA
ARG OPENCODE2_VERSION
ARG OPENCODE1_VERSION

FROM --platform=linux/amd64 rust:1-bookworm AS aft-builder
ARG AFT_GIT_SHA
ARG CARGO_PROFILE=release
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY benchmarks/aft-search/engine-fixtures/plan-table.json benchmarks/aft-search/engine-fixtures/plan-table.json
RUN test -n "$AFT_GIT_SHA" && cargo build --locked --profile "$CARGO_PROFILE" -p agent-file-tools
RUN mkdir -p /out && cp "/build/target/$CARGO_PROFILE/aft" /out/aft.real && \
    printf '%s\n' \
      '#!/bin/sh' \
      'if [ "${1:-}" = "--version" ] || [ "${1:-}" = "-V" ]; then' \
      "  /opt/aft/bin/aft.real \"\$@\" | sed 's/\$/ ($AFT_GIT_SHA)/'" \
      '  exit ${PIPESTATUS:-0}' \
      'fi' \
      'exec /opt/aft/bin/aft.real "$@"' > /out/aft && \
    chmod +x /out/aft /out/aft.real && \
    SHA256="$(sha256sum /out/aft | cut -d ' ' -f 1)" && \
    printf '{"git_sha":"%s","build_profile":"%s","sha256":"%s","source":"checkout-build"}\n' \
      "$AFT_GIT_SHA" "$CARGO_PROFILE" "$SHA256" > /out/build-info.json

FROM --platform=linux/amd64 node:24-bookworm AS plugin-builder
WORKDIR /src
RUN curl -fsSL https://bun.sh/install | bash
ENV PATH="/root/.bun/bin:$PATH"
COPY . .
RUN bun install --frozen-lockfile && \
    bun run --cwd packages/aft-bridge build && \
    bun run --cwd packages/opencode-plugin build && \
    mkdir -p /packs && npm pack ./packages/opencode-plugin --pack-destination /packs >/dev/null

FROM --platform=linux/amd64 node:24-bookworm-slim AS runtime
ARG OPENCODE2_VERSION
ARG OPENCODE1_VERSION
RUN test -n "$OPENCODE2_VERSION" && test -n "$OPENCODE1_VERSION" && \
    apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git procps unzip && \
    rm -rf /var/lib/apt/lists/* && \
    curl -fsSL https://bun.sh/install | bash && \
    npm install --prefix /opt/opencode --no-audit --no-fund \
      "@copilotkit/aimock@1.24.0" \
      "@opencode-ai/cli@$OPENCODE2_VERSION" \
      "@opencode-ai/cli-linux-x64@$OPENCODE2_VERSION" && \
    npm install --prefix /opt/opencode1 --no-audit --no-fund "opencode-ai@$OPENCODE1_VERSION"
ENV PATH="/root/.bun/bin:/opt/aft/bin:$PATH"
ENV NODE_PATH="/opt/opencode/node_modules"
WORKDIR /workspace
COPY . /workspace
COPY --from=plugin-builder /packs/*.tgz /opt/aft/packages/aft-opencode.tgz
COPY --from=aft-builder /out/ /opt/aft/bin/
ENV AFT_BINARY_PATH=/opt/aft/bin/aft
ENV AFT_OPENCODE2_PLUGIN_TARBALL=/opt/aft/packages/aft-opencode.tgz
ENV OPENCODE2_BIN=/opt/opencode/node_modules/@opencode-ai/cli-linux-x64/bin/opencode2
ENV OPENCODE1_BIN=/opt/opencode1/node_modules/.bin/opencode
ENTRYPOINT ["bun", "tests/docker/opencode2/harness/driver.ts"]
