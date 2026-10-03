# syntax=docker/dockerfile:1.7
# Upstream builds from the Fiber source tree:
#   https://github.com/nervosnetwork/fiber/tree/develop/docker
# This repo is Twine, so the builder checks out that tree and compiles it
# the same way (fiber-bin -> fnn, fnn-cli). Config templates come from the
# same checkout, which keeps config/mainnet/config.yml a real path.

FROM rust:1.95.0-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        git \
        libclang-dev \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/fiber
ENV CARGO_TARGET_DIR=/usr/src/fiber/target

# develop's rust-toolchain.toml is 1.95.0, matching this image, and also lists
# the wasm target. Install that target up front. Leave the toolchain file in
# the tree: deleting it makes git-version stamp fnn as dirty.
ARG FIBER_REPO=https://github.com/nervosnetwork/fiber.git
ARG FIBER_REF=develop
RUN git clone --depth 1 --branch "${FIBER_REF}" "${FIBER_REPO}" . \
    && rustup target add wasm32-unknown-unknown

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked -p fiber-bin -p fnn-cli

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        libstdc++6 \
        tini \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /fiber
ENV FIBER_HOME=/fiber

COPY --from=builder /usr/src/fiber/target/release/fnn /usr/local/bin/fnn
COPY --from=builder /usr/src/fiber/target/release/fnn-cli /usr/local/bin/fnn-cli
COPY --from=builder /usr/src/fiber/config /usr/local/share/fiber/config
COPY docker/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

RUN chmod +x /usr/local/bin/docker-entrypoint.sh

EXPOSE 8227 8228
VOLUME ["/fiber"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
CMD []
