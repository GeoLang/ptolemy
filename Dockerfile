FROM rust:bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -r -s /bin/false ptolemy

COPY --from=builder /app/target/release/ptolemy /usr/local/bin/ptolemy

USER ptolemy

ENV RUST_LOG=info,ptolemy=debug
ENV PTOLEMY_PORT=3000

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:3000/api/v1/health || exit 1

ENTRYPOINT ["ptolemy"]
CMD ["serve"]
