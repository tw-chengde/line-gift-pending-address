# bookworm 與下方 distroless debian12 的 glibc 版本一致。
FROM rust:1-slim-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /app/target/release/line-gift-pending-address /line-gift-pending-address
ENTRYPOINT ["/line-gift-pending-address"]
