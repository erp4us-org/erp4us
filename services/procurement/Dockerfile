FROM rust:1.100.0-bookworm@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea AS chef
RUN cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path /recipe.json

FROM chef AS build
COPY --from=planner /recipe.json /recipe.json
RUN cargo chef cook --locked --release -p procurement --recipe-path /recipe.json
COPY . .
RUN cargo build --locked --release -p procurement

FROM debian:bookworm-20261104-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
# Remove these pins when a reviewed base includes the PAM and zstd fixes.
RUN apt-get update \
    && apt-get install -y --no-install-recommends --only-upgrade \
        libpam-modules=1.5.2-6+deb12u3 libpam-modules-bin=1.5.2-6+deb12u3 \
        libpam-runtime=1.5.2-6+deb12u3 libpam0g=1.5.2-6+deb12u3 \
        libzstd1=1.5.4+dfsg2-5+deb12u1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/procurement /usr/local/bin/procurement
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/procurement"]
