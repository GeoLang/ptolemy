FROM rust:bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p ptolemy-cli

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/*

# amazon signs RDS certificates with its own roots, which no public root store
# carries, so verify-full against RDS needs this bundle named by sslrootcert
RUN curl -fsSL -o /etc/ssl/rds-global-bundle.pem \
    https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem \
    && chmod 0644 /etc/ssl/rds-global-bundle.pem

RUN useradd -r -s /bin/false ptolemy

COPY --from=builder /app/target/release/ptolemy /usr/local/bin/ptolemy

USER ptolemy

ENV RUST_LOG=info,ptolemy=debug

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:3000/api/v1/readyz || exit 1

ENTRYPOINT ["ptolemy"]
CMD ["serve"]
