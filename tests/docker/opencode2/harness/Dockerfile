# syntax=docker/dockerfile:1.7
ARG AFT_GIT_SHA
ARG OPENCODE2_VERSION
ARG OPENCODE1_VERSION
# Where the harness binary comes from: compile it here (the default, so a
# checkout alone is enough to run the matrix), or take the same-SHA artifact
# the caller staged into the build context. The policy admits both as
# provenance sources; the artifact path exists so CI does not pay a second
# release build of a binary it has already produced.
ARG AFT_BINARY_SOURCE=checkout-build

FROM --platform=linux/amd64 rust:1-bookworm AS aft-checkout-build
ARG AFT_GIT_SHA
ARG CARGO_PROFILE=release
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY benchmarks/aft-search/engine-fixtures/*.json benchmarks/aft-search/engine-fixtures/
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

# The caller stages the artifact directory (aft, aft.real, build-info.json)
# into the context under this name. Its build-info.json is the producer's,
# carrying source "same-sha-artifact"; nothing here writes or edits it.
FROM --platform=linux/amd64 node:24-bookworm-slim AS aft-prebuilt
COPY .aft-opencode2-artifact/ /out/
RUN test -f /out/aft && test -f /out/aft.real && test -f /out/build-info.json && \
    chmod +x /out/aft /out/aft.real

FROM aft-${AFT_BINARY_SOURCE} AS aft-builder

FROM --platform=linux/amd64 node:24-bookworm AS plugin-builder
WORKDIR /src
RUN curl -fsSL https://bun.sh/install | bash
ENV PATH="/root/.bun/bin:$PATH"
COPY . .
RUN bun install --frozen-lockfile && \
    bun run --cwd packages/aft-bridge build && \
    bun run --cwd packages/opencode-plugin build && \
    mkdir -p /packs /plugin/node_modules/@cortexkit/aft-opencode /plugin/node_modules && \
    npm pack ./packages/opencode-plugin --pack-destination /packs >/dev/null && \
    tar -xzf /packs/*.tgz -C /plugin/node_modules/@cortexkit/aft-opencode --strip-components=1 && \
    cp -a node_modules/.bun/effect@4.0.0-rc.112/node_modules/effect /plugin/node_modules/effect

FROM --platform=linux/amd64 node:24-bookworm-slim AS runtime
ARG OPENCODE2_VERSION
ARG OPENCODE1_VERSION
RUN test -n "$OPENCODE2_VERSION" && test -n "$OPENCODE1_VERSION" && \
    apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git procps unzip && \
    rm -rf /var/lib/apt/lists/* && \
    curl -fsSL https://bun.sh/install | bash && \
    npm install --prefix /opt/opencode --no-audit --no-fund \
      "@copilotkit/aimock@1.24.0" \
      "@opencode/cli@$OPENCODE2_VERSION" \
      "@opencode/cli-linux-x64@$OPENCODE2_VERSION" && \
    npm install --prefix /opt/opencode1 --no-audit --no-fund "opencode-ai@$OPENCODE1_VERSION"
ENV PATH="/root/.bun/bin:/opt/aft/bin:$PATH"
ENV NODE_PATH="/opt/opencode/node_modules"
WORKDIR /workspace
COPY . /workspace
COPY --from=plugin-builder /packs/*.tgz /opt/aft/packages/aft-opencode.tgz
COPY --from=plugin-builder /plugin/ /opt/aft/plugin/
COPY --from=aft-builder /out/ /opt/aft/bin/
ENV AFT_BINARY_PATH=/opt/aft/bin/aft
ENV AFT_E2E_NATIVE_BINARY_PATH=/opt/aft/bin/aft.real
ENV AFT_OPENCODE2_PLUGIN_TARBALL=/opt/aft/packages/aft-opencode.tgz
ENV AFT_OPENCODE2_PLUGIN_DIRECTORY=/opt/aft/plugin/node_modules/@cortexkit/aft-opencode
ENV OPENCODE2_BIN=/opt/opencode/node_modules/@opencode/cli-linux-x64/bin/opencode
ENV OPENCODE1_BIN=/opt/opencode1/node_modules/.bin/opencode
ENTRYPOINT ["bun", "tests/docker/opencode2/harness/driver.ts"]
