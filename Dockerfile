FROM rust:1.85-bookworm AS builder

RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
RUN cargo build --release --bin helyosd

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/helyosd /usr/local/bin/helyosd

RUN useradd -r -s /usr/sbin/nologin helyosd
USER helyosd

EXPOSE 6443 6444

ENTRYPOINT ["helyosd"]
