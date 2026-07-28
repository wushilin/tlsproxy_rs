FROM rust:1.88-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends clang libclang-dev cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libstdc++6 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/tlsproxy /usr/local/bin/tlsproxy
VOLUME ["/var/lib/tlsproxy"]
EXPOSE 443
ENTRYPOINT ["/usr/local/bin/tlsproxy"]
CMD ["run", "--runtime-dir", "/var/lib/tlsproxy"]
