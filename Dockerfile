# Build and run environment for the Bluetooth LE scanner (Linux).
# Installs Rust toolchain deps, BlueZ, and GUI libraries for egui/eframe.
FROM rust:bookworm

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    build-essential \
    clang \
    libclang-dev \
    libdbus-1-dev \
    libbluetooth-dev \
    bluez \
    bluez-tools \
    dbus \
    libssl-dev \
    libx11-dev \
    libxcursor-dev \
    libxrandr-dev \
    libxi-dev \
    libxkbcommon-dev \
    libxcb1-dev \
    libxcb-render0-dev \
    libxcb-shape0-dev \
    libxcb-xfixes0-dev \
    libgl1-mesa-dev \
    libgtk-3-dev \
    fontconfig \
    fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Cache crate dependencies before copying full sources
COPY Cargo.toml ./
RUN mkdir -p src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo fetch \
    && cargo build --release \
    && rm -rf src target/release/deps/bluetooth_scan* target/release/bluetooth-scan*

COPY src ./src
COPY Cargo.toml ./

RUN cargo build --release

# Default: build is already done; keep container ready for interactive use
CMD ["bash", "-c", "cargo build --release && sleep infinity"]
