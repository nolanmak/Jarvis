FROM ubuntu:24.04
ARG TOOLCHAIN=1.94.1
ENV RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo
ENV PATH=/opt/rust/cargo/bin:$PATH
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl build-essential pkg-config libssl-dev libdbus-1-dev \
    clang cmake git python3 util-linux && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -fsSf https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain "$TOOLCHAIN" \
    && chmod -R a+rX /opt/rust && rm /tmp/rustup.sh

COPY container-build.sh /usr/local/bin/jarvis-container-build
RUN chmod 755 /usr/local/bin/jarvis-container-build
