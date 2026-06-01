FROM rust:1.88-slim AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev git && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# Copy source and config
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY config.toml ./config.toml
COPY .gitmodules ./

# Clone submodules at the exact SHAs the repo's Cargo.lock was resolved
# against. Earlier this just ran `git submodule add` (clones HEAD), which
# silently picked up whatever upstream `main` was at on each build —
# breaking once `rain.math.float` bumped `wasm-bindgen` past our pinned
# `=0.2.100`. Pinning here matches the committed submodule pointers in
# `.gitmodules` / the working tree so Docker builds use the same sources
# CI / local builds do. When updating the submodules, bump these SHAs in
# lockstep.
ARG RAIN_MATH_FLOAT_SHA=1cf3969996be4cde836b77972b257b4eee7bd6d9
ARG RAIN_WASM_SHA=3a00563b25a59d709017873ff887e51f0b4ee738
RUN git init && \
    git submodule add https://github.com/rainlanguage/rain.math.float lib/rain.math.float && \
    git -C lib/rain.math.float checkout "$RAIN_MATH_FLOAT_SHA" && \
    git -C lib/rain.math.float submodule update --init --recursive && \
    git submodule add https://github.com/rainlanguage/rain.wasm lib/rain.wasm && \
    git -C lib/rain.wasm checkout "$RAIN_WASM_SHA" && \
    git -C lib/rain.wasm submodule update --init --recursive

# Place pre-built Solidity artifact where rain.math.float expects it
COPY artifacts/DecimalFloat.json lib/rain.math.float/out/DecimalFloat.sol/DecimalFloat.json

RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/st0x-oracle-server /usr/local/bin/
COPY config.toml /app/config.toml
WORKDIR /app
ENTRYPOINT ["st0x-oracle-server"]
