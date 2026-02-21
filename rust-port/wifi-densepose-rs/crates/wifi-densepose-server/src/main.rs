use std::{
    env,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Context};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderValue, Method, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use geo::Point;
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, sync::broadcast};
use tower_http::{
    cors::{AllowOrigin, Any, CorsLayer},
    trace::TraceLayer,
};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use uuid::Uuid;
use wifi_densepose_mat::{
    api::AppState as MatAppState,
    api::WebSocketMessage,
    domain::{SensorPosition, SensorType, SignalStrength},
    BreathingPattern, BreathingType, Coordinates3D, DisasterEvent, DisasterType,
    LocationUncertainty, MovementProfile, MovementType, ScanZone, VitalSignsReading,
};

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8787";
const DEFAULT_ALLOWED_ORIGINS: &str = "http://localhost:5173,http://127.0.0.1:5173";
const DEFAULT_POSE_HEARTBEAT_SECS: u64 = 2;

#[derive(Debug, Clone)]
struct ServerConfig {
    bind_addr: SocketAddr,
    allowed_origins: Vec<String>,
    pose_heartbeat_secs: u64,
}

impl ServerConfig {
    fn from_env() -> anyhow::Result<Self> {
        let bind_addr = env::var("WIFI_DENSEPOSE_BIND_ADDR")
            .unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string())
            .parse::<SocketAddr>()
            .context("failed to parse WIFI_DENSEPOSE_BIND_ADDR")?;

        let allowed_origins = env::var("WIFI_DENSEPOSE_ALLOWED_ORIGINS")
            .unwrap_or_else(|_| DEFAULT_ALLOWED_ORIGINS.to_string())
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();

        let pose_heartbeat_secs = env::var("WIFI_DENSEPOSE_POSE_HEARTBEAT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_POSE_HEARTBEAT_SECS)
            .max(1);

        Ok(Self {
            bind_addr,
            allowed_origins,
            pose_heartbeat_secs,
        })
    }
}

#[derive(Clone)]
struct ServerState {
    mat_state: MatAppState,
    pose_provider: PoseLocationProvider,
    demo_step: Arc<AtomicU64>,
}

#[derive(Clone)]
struct PoseLocationProvider {
    mat_state: MatAppState,
    tx: broadcast::Sender<PoseBroadcast>,
    frame_counter: Arc<AtomicU64>,
}

#[derive(Clone)]
enum PoseBroadcast {
    Frame(PoseFrame),
    Heartbeat(DateTime<Utc>),
}

#[derive(Debug, Clone, Serialize)]
struct PoseFrame {
    timestamp: DateTime<Utc>,
    frame_id: u64,
    coordinate_frame: String,
    persons: Vec<PosePerson>,
}

#[derive(Debug, Clone, Serialize)]
struct PosePerson {
    id: Uuid,
    confidence: f64,
    location_3d: PoseLocation3d,
}

#[derive(Debug, Clone, Serialize)]
struct PoseLocation3d {
    x: f64,
    y: f64,
    z: f64,
    uncertainty_radius: f64,
    confidence: f64,
}

#[derive(Debug, Serialize)]
struct PoseFrameWsEnvelope {
    #[serde(rename = "type")]
    message_type: &'static str,
    #[serde(flatten)]
    frame: PoseFrame,
}

#[derive(Debug, Serialize)]
struct PoseHeartbeatWsEnvelope {
    #[serde(rename = "type")]
    message_type: &'static str,
    timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct DemoSeedResponse {
    event_id: Uuid,
    step: u64,
    frame: PoseFrame,
}

#[derive(Debug, Deserialize)]
struct DemoSeedRequest {
    survivors: Option<usize>,
}

impl PoseLocationProvider {
    fn new(mat_state: MatAppState) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            mat_state,
            tx,
            frame_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<PoseBroadcast> {
        self.tx.subscribe()
    }

    fn current_frame(&self) -> PoseFrame {
        let persons = self
            .mat_state
            .list_events()
            .into_iter()
            .flat_map(|event| {
                event
                    .survivors()
                    .into_iter()
                    .filter_map(|survivor| {
                        let location = survivor.location()?;
                        Some(PosePerson {
                            id: *survivor.id().as_uuid(),
                            confidence: survivor.confidence(),
                            location_3d: PoseLocation3d {
                                x: location.x,
                                y: location.y,
                                z: location.z,
                                uncertainty_radius: location.uncertainty.horizontal_error,
                                confidence: location.uncertainty.confidence,
                            },
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        PoseFrame {
            timestamp: Utc::now(),
            frame_id: self.frame_counter.fetch_add(1, Ordering::Relaxed) + 1,
            coordinate_frame: "world_meters".to_string(),
            persons,
        }
    }

    fn broadcast_current_frame(&self) -> PoseFrame {
        let frame = self.current_frame();
        let _ = self.tx.send(PoseBroadcast::Frame(frame.clone()));
        frame
    }

    fn start_event_bridge(&self) {
        let provider = self.clone();
        let mut mat_rx = self.mat_state.subscribe();

        tokio::spawn(async move {
            loop {
                match mat_rx.recv().await {
                    Ok(message) => {
                        if should_emit_pose_frame(&message) {
                            provider.broadcast_current_frame();
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "Pose provider lagged behind MAT broadcast");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn start_heartbeat(&self, period: Duration) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            loop {
                interval.tick().await;
                let _ = tx.send(PoseBroadcast::Heartbeat(Utc::now()));
            }
        });
    }
}

fn should_emit_pose_frame(message: &WebSocketMessage) -> bool {
    matches!(
        message,
        WebSocketMessage::SurvivorDetected { .. }
            | WebSocketMessage::SurvivorUpdated { .. }
            | WebSocketMessage::SurvivorLost { .. }
            | WebSocketMessage::ZoneScanComplete { .. }
            | WebSocketMessage::EventStatusChanged { .. }
            | WebSocketMessage::Heartbeat { .. }
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = ServerConfig::from_env()?;
    let mat_state = MatAppState::new();
    let pose_provider = PoseLocationProvider::new(mat_state.clone());

    pose_provider.start_event_bridge();
    pose_provider.start_heartbeat(Duration::from_secs(config.pose_heartbeat_secs));

    let state = ServerState {
        mat_state: mat_state.clone(),
        pose_provider,
        demo_step: Arc::new(AtomicU64::new(0)),
    };

    let pose_router = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/pose/current", get(get_pose_current))
        .route("/ws/pose/stream", get(ws_pose_stream))
        .route("/api/v1/pose/demo/seed", post(seed_demo_data))
        .with_state(state);

    let app = Router::new()
        .merge(pose_router)
        .merge(wifi_densepose_mat::api::create_router(mat_state))
        .layer(TraceLayer::new_for_http())
        .layer(build_cors_layer(&config.allowed_origins)?);

    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.bind_addr))?;

    info!(
        bind_addr = %config.bind_addr,
        allowed_origins = ?config.allowed_origins,
        "wifi-densepose-server listening"
    );

    axum::serve(listener, app)
        .await
        .context("server exited with error")?;

    Ok(())
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
}

fn build_cors_layer(origins: &[String]) -> anyhow::Result<CorsLayer> {
    let parsed = origins
        .iter()
        .map(|origin| {
            origin
                .parse::<HeaderValue>()
                .with_context(|| format!("invalid CORS origin: {origin}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if parsed.is_empty() {
        return Err(anyhow!("at least one CORS origin must be configured"));
    }

    Ok(CorsLayer::new()
        .allow_origin(AllowOrigin::list(parsed))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn get_pose_current(State(state): State<ServerState>) -> Json<PoseFrame> {
    Json(state.pose_provider.current_frame())
}

async fn ws_pose_stream(State(state): State<ServerState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_pose_socket(socket, state.pose_provider))
}

async fn handle_pose_socket(socket: WebSocket, pose_provider: PoseLocationProvider) {
    let (mut sender, mut receiver) = socket.split();
    let mut pose_rx = pose_provider.subscribe();

    let send_task = tokio::spawn(async move {
        loop {
            let payload = match pose_rx.recv().await {
                Ok(PoseBroadcast::Frame(frame)) => serde_json::to_string(&PoseFrameWsEnvelope {
                    message_type: "pose_frame",
                    frame,
                }),
                Ok(PoseBroadcast::Heartbeat(timestamp)) => {
                    serde_json::to_string(&PoseHeartbeatWsEnvelope {
                        message_type: "heartbeat",
                        timestamp,
                    })
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "Pose WebSocket client lagged behind");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            let payload = match payload {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(%error, "failed to serialize pose websocket message");
                    continue;
                }
            };

            if sender.send(Message::Text(payload.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = receiver.next().await {
        if matches!(message, Message::Close(_)) {
            break;
        }
    }

    send_task.abort();
}

async fn seed_demo_data(
    State(state): State<ServerState>,
    payload: Option<Json<DemoSeedRequest>>,
) -> Result<Json<DemoSeedResponse>, (StatusCode, String)> {
    let requested_survivors = payload
        .map(|body| body.survivors.unwrap_or(2))
        .unwrap_or(2)
        .clamp(1, 8);

    let event_id = ensure_demo_event(&state.mat_state);
    let step = state.demo_step.fetch_add(1, Ordering::Relaxed) + 1;

    let update_result = state.mat_state.update_event(event_id, |event| {
        let zone_id = ensure_demo_zone(event);
        inject_demo_survivors(event, &zone_id, step, requested_survivors)
    });

    match update_result {
        Some(Ok(())) => {
            state.mat_state.broadcast(WebSocketMessage::Heartbeat {
                timestamp: Utc::now(),
            });
            let frame = state.pose_provider.broadcast_current_frame();
            Ok(Json(DemoSeedResponse {
                event_id,
                step,
                frame,
            }))
        }
        Some(Err(error)) => Err((StatusCode::INTERNAL_SERVER_ERROR, error)),
        None => Err((
            StatusCode::NOT_FOUND,
            "Demo event was not found".to_string(),
        )),
    }
}

fn ensure_demo_event(mat_state: &MatAppState) -> Uuid {
    if let Some(existing_id) = mat_state
        .list_events()
        .into_iter()
        .map(|event| *event.id().as_uuid())
        .next()
    {
        return existing_id;
    }

    let event = DisasterEvent::new(
        DisasterType::BuildingCollapse,
        Point::new(-122.4194, 37.7749),
        "Demo MAT event",
    );

    mat_state.store_event(event)
}

fn ensure_demo_zone(event: &mut DisasterEvent) -> wifi_densepose_mat::ScanZoneId {
    if let Some(zone) = event.zones().first() {
        return zone.id().clone();
    }

    let mut zone = ScanZone::new(
        "Demo Zone",
        wifi_densepose_mat::ZoneBounds::rectangle(0.0, 0.0, 25.0, 25.0),
    );

    zone.add_sensor(SensorPosition {
        id: "sensor-a".to_string(),
        x: 0.0,
        y: 0.0,
        z: 2.0,
        sensor_type: SensorType::Transceiver,
        is_operational: true,
    });
    zone.add_sensor(SensorPosition {
        id: "sensor-b".to_string(),
        x: 25.0,
        y: 0.0,
        z: 2.0,
        sensor_type: SensorType::Transceiver,
        is_operational: true,
    });
    zone.add_sensor(SensorPosition {
        id: "sensor-c".to_string(),
        x: 12.5,
        y: 20.0,
        z: 2.0,
        sensor_type: SensorType::Transceiver,
        is_operational: true,
    });

    let zone_id = zone.id().clone();
    event.add_zone(zone);
    zone_id
}

fn inject_demo_survivors(
    event: &mut DisasterEvent,
    zone_id: &wifi_densepose_mat::ScanZoneId,
    step: u64,
    survivors: usize,
) -> Result<(), String> {
    let t = step as f64 * 0.25;

    for idx in 0..survivors {
        let idxf = idx as f64;
        let x = 6.0 + idxf * 4.0 + (t + idxf).sin() * 1.8;
        let y = 6.0 + idxf * 3.0 + (t * 0.8 + idxf).cos() * 1.4;
        let z = -1.0 - ((t + idxf * 0.7).sin().abs() * 2.0);

        let location = Coordinates3D::new(x, y, z, LocationUncertainty::new(0.7, 0.5));
        let vitals = demo_vitals(idxf, t);

        event
            .record_detection(zone_id.clone(), vitals, Some(location))
            .map_err(|error| error.to_string())?;
    }

    Ok(())
}

fn demo_vitals(seed: f64, t: f64) -> VitalSignsReading {
    let breathing = BreathingPattern {
        rate_bpm: (14.0 + (seed + t).sin() * 4.0) as f32,
        amplitude: 0.8,
        regularity: 0.75,
        pattern_type: BreathingType::Normal,
    };

    let heartbeat = wifi_densepose_mat::HeartbeatSignature {
        rate_bpm: (72.0 + (seed + t).cos() * 9.0) as f32,
        variability: 0.12,
        strength: SignalStrength::Moderate,
    };

    let movement = MovementProfile {
        movement_type: if (t + seed).sin().abs() > 0.6 {
            MovementType::Fine
        } else {
            MovementType::None
        },
        intensity: 0.3,
        frequency: 0.2,
        is_voluntary: false,
    };

    VitalSignsReading::new(Some(breathing), Some(heartbeat), movement)
}
