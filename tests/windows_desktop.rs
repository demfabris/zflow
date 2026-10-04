//! Opt-in real Windows engine smoke test. No keyboard/mouse events or clipboard access.
#![cfg(windows)]

use serde_json::{Value, json};
use std::{net::UdpSocket, path::Path, process::Stdio, time::Duration};
use tokio::{process::Command, sync::mpsc, time::timeout};
use zflow::{
    config::Config,
    core::{ActivationId, SessionContext, SessionEpoch, TransportGeneration},
    desktop::{DesktopRequest, DesktopResponse, Geometry, Point, Rect},
    identity::Identity,
    session::{SessionEventKind, SessionOptions, start_session},
    transport::{self, Accepted},
    wire::{Hello, Os},
};

fn cli(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zflow"));
    command
        .args(["--config"])
        .arg(path)
        .creation_flags(0x08000000);
    command
}
async fn request(path: &Path, request: Value) -> Value {
    let output = timeout(
        Duration::from_secs(15),
        cli(path).arg("request").arg(request.to_string()).output(),
    )
    .await
    .unwrap()
    .unwrap();
    if !output.status.success() {
        return json!({"error":String::from_utf8_lossy(&output.stderr)});
    }
    serde_json::from_slice(&output.stdout).unwrap()
}
async fn state_when(path: &Path, predicate: impl Fn(&Value) -> bool) -> Value {
    timeout(Duration::from_secs(12), async {
        loop {
            let state = request(path, json!({"command":"status"})).await;
            if predicate(&state) {
                break state;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("engine did not reach the expected state")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an unlocked Windows desktop; does not capture or inject physical input"]
async fn native_engine_pairs_persists_reconnects_and_stops() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zflow.toml");
    let mut config = Config::default();
    config.daemon.state_dir = directory.path().join("state");
    config.transport.discovery = false;
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    config.transport.listen = format!("[::]:{port}").parse().unwrap();
    config.save(&path).unwrap();
    let log = std::fs::File::create(directory.path().join("engine.log")).unwrap();
    let mut engine = cli(&path)
        .arg("run")
        .stdout(Stdio::null())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            assert!(
                engine.try_wait().unwrap().is_none(),
                "engine exited: {}",
                std::fs::read_to_string(directory.path().join("engine.log")).unwrap()
            );
            if cli(&path)
                .arg("status")
                .output()
                .await
                .unwrap()
                .status
                .success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("engine startup timed out");
    let state = state_when(&path, |s| s["available"] == true).await;
    assert!(state["peers"].as_array().unwrap().is_empty());
    let our_key = state["key"].clone();
    let duplicate = cli(&path).arg("run").output().await.unwrap();
    assert!(
        !duplicate.status.success(),
        "a second engine must not own this configuration"
    );

    let peer = Identity::load_or_create(&directory.path().join("peer")).unwrap();
    let peer_key = peer.fingerprint_hex();
    let server =
        transport::input_server_config_for_peers(&peer, std::iter::empty::<&[u8]>()).unwrap();
    let endpoint =
        quinn::Endpoint::server(server.quinn_config(), "127.0.0.1:0".parse().unwrap()).unwrap();
    let address = endpoint.local_addr().unwrap();
    let listener = endpoint.clone();
    let (connected, mut sessions) = mpsc::channel(4);
    let accepting = tokio::spawn(async move {
        let mut server = server;
        while let Some(incoming) = listener.accept().await {
            match transport::accept(incoming, &server).await.unwrap() {
                Accepted::Hello(c) => {
                    server =
                        transport::input_server_config_for_peers(&peer, [c.peer_spki()]).unwrap();
                    listener.set_server_config(Some(server.quinn_config()));
                    let hello = c
                        .exchange(&Hello {
                            name: "smoke-peer".into(),
                            os: Os::Linux,
                            version: env!("CARGO_PKG_VERSION").into(),
                            input_port: address.port(),
                            candidates: vec![],
                            trusts_you: true,
                            vouches: [[0; 16]; 16],
                        })
                        .await
                        .unwrap();
                    assert_eq!(hello.os, Os::Windows);
                    assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));
                }
                Accepted::Input(c) => {
                    let (events, mut received) = mpsc::channel(64);
                    let handle = start_session(
                        c,
                        "windows".into(),
                        TransportGeneration(1),
                        SessionOptions::from_config(&Config::default()).unwrap(),
                        events,
                    )
                    .await
                    .unwrap();
                    connected.send(handle).await.unwrap();
                    tokio::spawn(async move {
                        while let Some(event) = received.recv().await {
                            match event.kind {
                                SessionEventKind::Desktop {
                                    request: DesktopRequest::Snapshot,
                                    reply,
                                } => {
                                    let _ = reply.send(DesktopResponse::Snapshot {
                                        geometry: Geometry {
                                            monitors: vec![Rect {
                                                x: 0,
                                                y: 0,
                                                width: 1920,
                                                height: 1080,
                                            }],
                                        },
                                        position: Point { x: 500, y: 500 },
                                    });
                                }
                                SessionEventKind::Desktop { reply, .. } => {
                                    let _ = reply.send(DesktopResponse::unavailable(
                                        "Smoke test never activates input",
                                    ));
                                }
                                SessionEventKind::ReceiverEffects { applied, .. } => {
                                    let _ =
                                        applied.send(Err("Smoke test never injects input".into()));
                                }
                                _ => {}
                            }
                        }
                    });
                }
                other => panic!("unexpected admission result: {other:?}"),
            }
        }
    });
    let nearby = request(
        &path,
        json!({"command":"nearby","address":address.to_string()}),
    )
    .await;
    assert!(nearby["error"].is_null(), "{nearby}");
    assert_eq!(nearby["nearby"][0]["id"], format!("key:{peer_key}"));
    let trusted = request(&path, json!({"command":"trust","key":peer_key})).await;
    assert!(trusted["error"].is_null(), "{trusted}");
    let state = state_when(&path, |s| s["peers"][0]["connected"] == true).await;
    let remote = timeout(Duration::from_secs(3), sessions.recv())
        .await
        .unwrap()
        .unwrap();
    let snapshot = remote
        .desktop_request(DesktopRequest::Snapshot)
        .await
        .unwrap();
    assert!(matches!(snapshot, DesktopResponse::Snapshot { .. }));

    let mut layout = state["layout"].clone();
    let local_width = layout["monitors"][0]["width"].as_i64().unwrap();
    // Leave a gap so real physical pointer motion cannot cross during this test.
    let monitors = layout["monitors"].as_array_mut().unwrap();
    let tile = monitors
        .iter_mut()
        .find(|m| m["peer"] == "smoke-peer")
        .unwrap();
    tile["x"] = json!(local_width + 128);
    let arrange = json!({"command":"arrange","layout":layout,"version":state["layout_version"]});
    let arranged = request(&path, arrange.clone()).await;
    assert!(arranged["error"].is_null(), "{arranged}");
    assert!(
        !request(&path, arrange).await["error"].is_null(),
        "stale arrangements must be rejected"
    );
    for setting in [
        json!({"command":"keyboard","peer":"smoke-peer","mode":"pc-positions"}),
        json!({"command":"reverse_scroll","peer":"smoke-peer","enabled":true}),
        json!({"command":"pause_at_edges","enabled":true}),
    ] {
        let result = request(&path, setting).await;
        assert!(result["error"].is_null(), "{result}");
    }
    let saved = Config::load(&path).unwrap();
    assert!(saved.peers["smoke-peer"].reverse_scroll);
    assert!(saved.switching.pause_at_edges);
    assert!(!saved.clipboard.share);
    // Enter with neutral state only: no key, button, motion, or clipboard data.
    // Return input here must stop the sender, not just clear current held keys.
    remote
        .begin_outbound(SessionContext {
            session_epoch: SessionEpoch([7; 16]),
            transport_generation: remote.generation(),
            activation_id: ActivationId(1),
        })
        .unwrap();
    state_when(&path, |s| s["receiving"] == "smoke-peer").await;
    request(&path, json!({"command":"local"})).await;
    timeout(Duration::from_secs(3), async {
        while !remote.is_closed() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Return input here must close the remote sender's session");
    state_when(&path, |s| s["peers"][0]["connected"] == true).await;
    request(&path, json!({"command":"sharing","enabled":false})).await;
    assert!(request(&path, json!({"command":"status"})).await["peers"][0]["connected"] == false);
    request(&path, json!({"command":"sharing","enabled":true})).await;
    state_when(&path, |s| s["peers"][0]["connected"] == true).await;
    request(&path, json!({"command":"forget","peer":"smoke-peer"})).await;
    assert!(Config::load(&path).unwrap().peers.is_empty());
    request(&path, json!({"command":"quit"})).await;
    assert!(
        timeout(Duration::from_secs(5), engine.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let reloaded = Identity::load_or_create(&config.daemon.state_dir).unwrap();
    assert_eq!(our_key, reloaded.fingerprint_hex());
    endpoint.close(0u32.into(), b"smoke test finished");
    accepting.abort();
}
