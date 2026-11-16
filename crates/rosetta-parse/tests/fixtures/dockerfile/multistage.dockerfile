# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.98.1
ARG BASE=debian
ARG UNSET_ARG

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-trixie AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY ./scripts/build.sh /usr/local/bin/build.sh
COPY --chown=1000:1000 config/app.yaml /etc/app/app.yaml
COPY src/*.rs /tmp/glob/
COPY . .
ADD https://example.com/tool.tgz /tmp/
ADD /abs/path /tmp/abs
ADD ["spaced name.txt", "data/x.bin", "/dest/"]
RUN cargo build --release \
    # a comment inside the continuation
    && strip target/release/app

FROM ${BASE}:trixie-slim AS runtime
COPY --from=builder /src/target/release/app /usr/local/bin/app
COPY --from=0 /etc/ssl /etc/ssl
COPY --from=docker.io/library/busybox:1 /bin/busybox /bin/busybox
EXPOSE 8080 9090/udp 5000/tcp
EXPOSE 8000-8010
CMD ["app", "--help"]
ENTRYPOINT ["app", "serve"]

FROM runtime AS debug
RUN <<EOF
cat <<INNER
COPY not-an-instruction /x
INNER
EOF
COPY <<EOT /etc/motd
hello
EOT
EXPOSE 2345
CMD sleep infinity

FROM scratch
COPY --from=runtime /usr/local/bin/app /app
