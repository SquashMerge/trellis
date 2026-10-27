FROM ubuntu:20.04@sha256:8feb4d8ca5354def3d8fce243717141ce31e2c428701f6682bd2fafe15388214
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git pkg-config python3 binutils procps \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH
# rustup installs the toolchain specified by the checked-out rust-toolchain.toml.
RUN curl --proto '=https' --tlsv1.2 -sSf \
      "https://static.rust-lang.org/rustup/archive/1.29.2/$(uname -m)-unknown-linux-gnu/rustup-init" \
      -o /tmp/rustup-init \
    && chmod +x /tmp/rustup-init \
    && /tmp/rustup-init -y --no-modify-path --default-toolchain none \
    && rm /tmp/rustup-init
WORKDIR /work
