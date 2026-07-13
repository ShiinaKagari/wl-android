FROM debian:bookworm-slim AS builder

RUN apt-get update && apt-get install -y \
    curl \
    build-essential \
    pkg-config \
    libwayland-dev \
    libdrm-dev \
    libvulkan-dev \
    mesa-vulkan-drivers \
    cmake \
    ninja-build \
    python3 \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    && /root/.cargo/bin/rustup target add x86_64-unknown-linux-gnu

ENV PATH="/root/.cargo/bin:${PATH}"
ENV CC=gcc
ENV CXX=g++

WORKDIR /build
COPY . .

RUN cargo build --target x86_64-unknown-linux-gnu --release -p land-common \
    && cargo build --target x86_64-unknown-linux-gnu --release -p land \
    && cp target/x86_64-unknown-linux-gnu/release/libland_wlroots.so /output/
