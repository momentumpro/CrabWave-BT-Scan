# Bluetooth Scan

Desktop BLE scanner written in Rust. Scans nearby Bluetooth Low Energy devices and shows name, MAC address, signal strength, and optional OUI vendor lookup.

## Main features

- **BLE scan** via [`btleplug`](https://crates.io/crates/btleplug) (Windows / Linux / macOS)
- **Simple UI** with [`egui`](https://crates.io/crates/egui) / `eframe`
- Device **name**, **type hints** (services / manufacturer / CoD), **MAC**, **RSSI**, proximity band
- Sort by signal strength; optional “nearby only” filter
- Copy MAC to clipboard
- Optional **OUI vendor** lookup (`api.maclookup.app`)

## Requirements

- Rust toolchain (1.88+ recommended)
- System Bluetooth enabled
- On Linux: BlueZ + D-Bus (also installed in the Docker image)

## Build (host)

```bash
cargo build --release
```

## Run (host)

```bash
cargo run --release
```

Turn Bluetooth on before scanning. On Windows this is the recommended way to use the app (UI + radio access).

## Docker

The image installs Rust, BlueZ, and GUI build dependencies, then compiles the project.

```bash
# Build image + start container (keeps deps ready)
docker compose up -d --build

# Rebuild inside the running container
docker compose exec bluetooth-scan cargo build --release
```

**Note:** On Windows Docker Desktop, Bluetooth hardware is usually not available inside the container. Use host `cargo run` for real scans. Docker is mainly for a reproducible Linux dependency/build environment.

## Project layout

| Path | Purpose |
|------|---------|
| `src/main.rs` | UI + scan logic |
| `Cargo.toml` | Crate dependencies |
| `Dockerfile` | Linux build environment |
| `docker-compose.yml` | Dev container with cargo caches |
