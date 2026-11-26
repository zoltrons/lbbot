FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY migrations migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates fonts-dejavu-core fonts-wqy-zenhei && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/lbbot /usr/local/bin/lbbot
USER nobody
CMD ["lbbot"]
