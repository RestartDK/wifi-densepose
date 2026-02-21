# wifi-densepose-server

Rust Axum server that composes:

- MAT REST + WebSocket API from `wifi-densepose-mat`
- Pose location REST + WebSocket API backed by MAT survivor locations
- Demo seeding endpoint for end-to-end local validation

## Endpoints

- `GET /api/v1/mat/events`
- `GET /ws/mat/stream`
- `GET /api/v1/pose/current`
- `GET /ws/pose/stream`
- `POST /api/v1/pose/demo/seed`

## Run

```bash
cargo run -p wifi-densepose-server
```

One-command local demo (run server, seed, start static dashboard, open browser):

```bash
make -C rust-port/wifi-densepose-rs pose-demo
```

Defaults:

- bind address: `127.0.0.1:8787`
- CORS origins: `http://localhost:5173,http://127.0.0.1:5173`

Environment overrides:

- `WIFI_DENSEPOSE_BIND_ADDR`
- `WIFI_DENSEPOSE_ALLOWED_ORIGINS`
- `WIFI_DENSEPOSE_POSE_HEARTBEAT_SECS`
- `WIFI_DENSEPOSE_DASHBOARD_PORT` (only used by `scripts/pose-demo.sh`)

## Test

```bash
cargo test -p wifi-densepose-server --test pose_contract
```

## Demo Mode -> Real Mode Roadmap

Demo mode uses `POST /api/v1/pose/demo/seed` to generate synthetic survivors with moving `{x,y,z}`.

For real mode, the expected handoff is:

1. Feed real RSSI/CSI measurements through MAT integration adapters under
   `crates/wifi-densepose-mat/src/integration/`.
2. Use MAT localization fusion in
   `crates/wifi-densepose-mat/src/localization/fusion.rs` to produce authoritative
   `Coordinates3D` per survivor.
3. Keep `PoseLocationProvider` as a thin translation layer that exposes MAT survivors
   through `/api/v1/pose/current` and `/ws/pose/stream`.
4. Move zone + sensor position configuration from demo seeding to config/db-backed
   persistence.
