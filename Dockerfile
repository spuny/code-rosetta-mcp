FROM rust:1.98.1-slim-trixie AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked
COPY scripts/license_bundle.py ./scripts/license_bundle.py
COPY LICENSES ./LICENSES
RUN python3 scripts/license_bundle.py target/third-party

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 rosetta \
 && mkdir /data \
 && chown rosetta:rosetta /data
COPY --from=build /src/target/release/rosetta /usr/local/bin/rosetta
COPY --from=build /src/target/third-party /usr/share/doc/rosetta/dependencies
COPY LICENSE THIRD_PARTY.md /usr/share/doc/rosetta/
COPY LICENSES /usr/share/doc/rosetta/LICENSES
USER 10001
WORKDIR /data
ENV ROSETTA_DATABASE=/data/graph.db
EXPOSE 3000
ENTRYPOINT ["rosetta"]
CMD ["serve", "--transport", "stdio"]
