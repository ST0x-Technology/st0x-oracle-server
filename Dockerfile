FROM rust:1.88-slim AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev git && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# Copy source and config
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY .gitmodules ./

# Clone submodules directly (avoids needing them initialized locally)
RUN git init && \
    git submodule add https://github.com/rainlanguage/rain.math.float lib/rain.math.float && \
    git submodule add https://github.com/rainlanguage/rain.wasm lib/rain.wasm

# Place pre-built Solidity artifact where rain.math.float expects it
COPY artifacts/DecimalFloat.json lib/rain.math.float/out/DecimalFloat.sol/DecimalFloat.json

RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/st0x-oracle-server /usr/local/bin/
ENTRYPOINT ["st0x-oracle-server"]
