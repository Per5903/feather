# Образ feather-server. Собирается из корня репозитория:
#   docker build -f server/Dockerfile -t feather-server .
FROM rust:1.99-slim-trixie AS build
WORKDIR /src
COPY . .
# Без BuildKit-кэшей: собирается и на старом Docker без buildx (типичный VPS)
RUN cargo build --release --locked -p feather-server \
    && cp target/release/feather-server /feather-server

FROM debian:trixie-slim
RUN useradd --system --uid 10001 feather && mkdir /data && chown feather /data
COPY --from=build /feather-server /usr/local/bin/feather-server
USER feather
ENV FEATHER_BIND=0.0.0.0:8080 \
    FEATHER_DATA_DIR=/data \
    RUST_LOG=feather_server=info
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --retries=5 CMD ["/usr/local/bin/feather-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/feather-server"]
