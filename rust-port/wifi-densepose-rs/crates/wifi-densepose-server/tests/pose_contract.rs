use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PoseFrame {
    frame_id: u64,
    coordinate_frame: String,
    persons: Vec<PosePerson>,
}

#[derive(Debug, Deserialize)]
struct PosePerson {
    location_3d: PoseLocation3d,
}

#[derive(Debug, Deserialize)]
struct PoseLocation3d {
    z: f64,
}

#[derive(Debug, Deserialize)]
struct SeedResponse {
    event_id: String,
    step: u64,
    frame: PoseFrame,
}

fn pick_unused_port() -> anyhow::Result<u16> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("failed to bind local probe port")?;
    let port = listener
        .local_addr()
        .context("failed to read probe socket address")?
        .port();
    Ok(port)
}

fn spawn_server(bind_addr: &str) -> anyhow::Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_wifi-densepose-server"))
        .env("WIFI_DENSEPOSE_BIND_ADDR", bind_addr)
        .env(
            "WIFI_DENSEPOSE_ALLOWED_ORIGINS",
            "http://127.0.0.1:5173,http://localhost:5173",
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn wifi-densepose-server on {bind_addr}"))
}

async fn wait_for_healthz(client: &reqwest::Client, base_url: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let url = format!("{base_url}/healthz");

    while Instant::now() < deadline {
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    bail!("server did not become healthy at {url} before timeout")
}

#[tokio::test]
async fn pose_seed_then_current_contract_is_stable() -> anyhow::Result<()> {
    let port = pick_unused_port()?;
    let bind_addr = format!("127.0.0.1:{port}");
    let base_url = format!("http://{bind_addr}");

    let mut server = spawn_server(&bind_addr)?;

    let result = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .context("failed to build HTTP client")?;

        wait_for_healthz(&client, &base_url).await?;

        let seed_response = client
            .post(format!("{base_url}/api/v1/pose/demo/seed"))
            .json(&serde_json::json!({ "survivors": 3 }))
            .send()
            .await
            .context("POST /api/v1/pose/demo/seed failed")?;

        if !seed_response.status().is_success() {
            bail!(
                "seed endpoint returned non-success status: {}",
                seed_response.status()
            );
        }

        let seeded: SeedResponse = seed_response
            .json()
            .await
            .context("failed to parse seed response JSON")?;

        assert!(!seeded.event_id.is_empty());
        assert_eq!(seeded.step, 1);
        assert_eq!(seeded.frame.coordinate_frame, "world_meters");
        assert_eq!(seeded.frame.persons.len(), 3);

        let current_response = client
            .get(format!("{base_url}/api/v1/pose/current"))
            .send()
            .await
            .context("GET /api/v1/pose/current failed")?;

        if !current_response.status().is_success() {
            bail!(
                "pose current endpoint returned non-success status: {}",
                current_response.status()
            );
        }

        let current: PoseFrame = current_response
            .json()
            .await
            .context("failed to parse current frame JSON")?;

        assert!(current.frame_id >= seeded.frame.frame_id);
        assert_eq!(current.coordinate_frame, "world_meters");
        assert!(!current.persons.is_empty());
        assert!(current
            .persons
            .iter()
            .all(|person| person.location_3d.z <= 0.0));

        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = server.kill();
    let _ = server.wait();

    result
}
