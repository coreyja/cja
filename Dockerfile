FROM rust:1-bookworm AS builder
WORKDIR /app

COPY . .

RUN cargo build --release --package cja-site

# Build rustdoc (no --release needed — output goes to target/doc/ regardless)
RUN RUSTDOCFLAGS="--html-in-header crates/cja.app/assets/docs-header.html" \
    cargo doc --no-deps --package cja

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/cja-site /usr/local/bin/cja-site
COPY --from=builder /app/target/doc/ /app/docs/

ENV DOCS_PATH=/app/docs
ENV PORT=8080

EXPOSE 8080
CMD ["cja-site"]
