use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use zflow::{
    core::{
        ActivationId, ControlSequence, CumulativeMotion, MonotonicTimeMicros, MotionFrame,
        MotionSequence, ReliableControl, ReliableControlMessage, SessionContext, SessionEpoch,
        TransportGeneration,
    },
    identity::Identity,
    transport::{
        INPUT_CHANNELS, InputChannelKind, InputControlMessage, InputDatagram, TransportError,
        accept_input, accept_pairing, connect_input, connect_pairing, input_client_config,
        input_server_config, input_server_config_for_peers, pairing_client_config,
        pairing_server_config,
    },
    wire::{CURRENT_PROTOCOL_VERSION, PairingMethod, PairingOffer, WireMessage, encode},
};

const LOOPBACK: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

fn identity() -> (tempfile::TempDir, Identity) {
    let directory = tempfile::tempdir().unwrap();
    let identity = Identity::load_or_create(directory.path()).unwrap();
    (directory, identity)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_input_listener_authenticates_every_allowed_peer_and_rejects_an_unknown_key() {
    let (_server_directory, server_identity) = identity();
    let (_first_directory, first_identity) = identity();
    let (_second_directory, second_identity) = identity();
    let (_unknown_directory, unknown_identity) = identity();

    assert!(
        input_server_config_for_peers(&server_identity, std::iter::empty::<&'static [u8]>())
            .is_err()
    );
    let server_config = input_server_config_for_peers(
        &server_identity,
        [first_identity.spki(), second_identity.spki()],
    )
    .unwrap();
    let first_config = input_client_config(&first_identity, server_identity.spki()).unwrap();
    let second_config = input_client_config(&second_identity, server_identity.spki()).unwrap();
    let unknown_config = input_client_config(&unknown_identity, server_identity.spki()).unwrap();

    let server_endpoint = quinn::Endpoint::server(server_config.quinn_config(), LOOPBACK).unwrap();
    let server_address = server_endpoint.local_addr().unwrap();
    let accept_endpoint = server_endpoint.clone();
    let accept_config = server_config.clone();
    let accept = tokio::spawn(async move {
        let mut accepted = Vec::new();
        for _ in 0..3 {
            let incoming = accept_endpoint.accept().await.unwrap();
            accepted.push(accept_input(incoming, &accept_config).await);
        }
        accepted
    });

    let first_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let second_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let unknown_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let (first, second, unknown) = tokio::join!(
        connect_input(&first_endpoint, server_address, &first_config),
        connect_input(&second_endpoint, server_address, &second_config),
        connect_input(&unknown_endpoint, server_address, &unknown_config),
    );
    let first = first.unwrap();
    let second = second.unwrap();

    let accepted = tokio::time::timeout(Duration::from_secs(2), accept)
        .await
        .expect("server did not finish authenticating three handshakes")
        .unwrap();
    let mut accepted_spkis = Vec::new();
    let mut rejected = 0;
    let mut server_connections = Vec::new();
    for result in accepted {
        match result {
            Ok(connection) => {
                accepted_spkis.push(connection.peer_spki().to_vec());
                server_connections.push(connection);
            }
            Err(_) => rejected += 1,
        }
    }
    accepted_spkis.sort();
    let mut expected = vec![
        first_identity.spki().to_vec(),
        second_identity.spki().to_vec(),
    ];
    expected.sort();
    assert_eq!(accepted_spkis, expected);
    assert_eq!(rejected, 1);

    if let Ok(unknown) = unknown {
        tokio::time::timeout(Duration::from_secs(2), unknown.closed())
            .await
            .expect("unknown peer remained connected after TLS rejection");
    }

    first.close();
    second.close();
    for connection in server_connections {
        connection.close();
    }
    first_endpoint.wait_idle().await;
    second_endpoint.wait_idle().await;
    unknown_endpoint.wait_idle().await;
    server_endpoint.wait_idle().await;
}

fn session() -> SessionContext {
    SessionContext {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        session_epoch: SessionEpoch([0x42; 16]),
        transport_generation: TransportGeneration(1),
        activation_id: ActivationId(1),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutual_rpk_pinning_delivers_control_and_datagram_without_bulk_channel() {
    let (_client_directory, client_identity) = identity();
    let (_server_directory, server_identity) = identity();
    let client_config = input_client_config(&client_identity, server_identity.spki()).unwrap();
    let server_config = input_server_config(&server_identity, client_identity.spki()).unwrap();

    let server_endpoint = quinn::Endpoint::server(server_config.quinn_config(), LOOPBACK).unwrap();
    let server_address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let accept_endpoint = server_endpoint.clone();
    let accept_config = server_config.clone();
    let accept = tokio::spawn(async move {
        let incoming = accept_endpoint.accept().await.unwrap();
        accept_input(incoming, &accept_config).await.unwrap()
    });

    let mut client = connect_input(&client_endpoint, server_address, &client_config)
        .await
        .unwrap();
    let mut server = accept.await.unwrap();
    assert_eq!(client.peer_spki(), server_identity.spki());
    assert_eq!(server.peer_spki(), client_identity.spki());

    let control = ReliableControlMessage {
        session: session(),
        sequence: ControlSequence(1),
        payload: ReliableControl::Enter,
    };
    client
        .channels_mut()
        .0
        .send_control(&control)
        .await
        .unwrap();
    assert_eq!(
        server.channels_mut().1.receive().await.unwrap(),
        InputControlMessage::Reliable(control)
    );

    // The negotiated application limit is checked independently of Quinn's
    // path limit on both send and receive.
    let motion = MotionFrame {
        session: session(),
        motion_sequence: MotionSequence(1),
        control_watermark: ControlSequence(1),
        sender_capture_time: MonotonicTimeMicros(50),
        totals: CumulativeMotion::new(12, -4, 3, 0),
        touch_snapshot: None,
    };
    assert!(matches!(
        client.channels_mut().2.send_motion(&motion),
        Err(TransportError::DatagramSizeNotNegotiated)
    ));
    client.channels_mut().2.configure_maximum(512).unwrap();
    server.channels_mut().2.configure_maximum(512).unwrap();
    assert!(matches!(
        client.channels_mut().2.configure_maximum(511),
        Err(TransportError::DatagramSizeAlreadyNegotiated {
            current: 512,
            requested: 511,
        })
    ));
    client.channels_mut().2.send_motion(&motion).unwrap();
    assert_eq!(
        server.channels_mut().2.receive().await.unwrap(),
        InputDatagram::Motion(motion)
    );

    assert_eq!(
        INPUT_CHANNELS,
        [
            InputChannelKind::ReliableControl,
            InputChannelKind::CumulativeMotion,
            InputChannelKind::Probe,
        ]
    );

    client.close();
    server.close();
    client_endpoint.wait_idle().await;
    server_endpoint.wait_idle().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_fragmented_control_reads_preserves_framing_progress() {
    let (_client_directory, client_identity) = identity();
    let (_server_directory, server_identity) = identity();
    let client_config = input_client_config(&client_identity, server_identity.spki()).unwrap();
    let server_config = input_server_config(&server_identity, client_identity.spki()).unwrap();

    let server_endpoint = quinn::Endpoint::server(server_config.quinn_config(), LOOPBACK).unwrap();
    let server_address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let accept_endpoint = server_endpoint.clone();
    let accept_config = server_config.clone();
    let accept = tokio::spawn(async move {
        let incoming = accept_endpoint.accept().await.unwrap();
        accept_input(incoming, &accept_config).await.unwrap()
    });

    // Open the authenticated stream directly so the test controls fragment
    // boundaries that the typed sender deliberately hides.
    let raw_connection = client_endpoint
        .connect_with(
            client_config.quinn_config(),
            server_address,
            "zflow.invalid",
        )
        .unwrap()
        .await
        .unwrap();
    let (mut raw_send, _raw_receive) = raw_connection.open_bi().await.unwrap();
    raw_send.write_all(b"zflow-control-v1\0").await.unwrap();
    let server = accept.await.unwrap();
    let mut server_channels = server.into_channels();

    let control = ReliableControlMessage {
        session: session(),
        sequence: ControlSequence(1),
        payload: ReliableControl::Enter,
    };
    let payload = encode(&WireMessage::ReliableControl(control.clone())).unwrap();
    let length = u32::try_from(payload.len()).unwrap().to_be_bytes();

    raw_send.write_all(&length[..2]).await.unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            server_channels.control_receive.receive(),
        )
        .await
        .is_err()
    );

    raw_send.write_all(&length[2..]).await.unwrap();
    raw_send.write_all(&payload[..2]).await.unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            server_channels.control_receive.receive(),
        )
        .await
        .is_err()
    );

    raw_send.write_all(&payload[2..]).await.unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            server_channels.control_receive.receive(),
        )
        .await
        .unwrap()
        .unwrap(),
        InputControlMessage::Reliable(control)
    );

    raw_connection.close(0_u32.into(), b"");
    client_endpoint.wait_idle().await;
    server_endpoint.wait_idle().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unread_control_stream_hits_the_bounded_fail_closed_write_timeout() {
    let (_client_directory, client_identity) = identity();
    let (_server_directory, server_identity) = identity();
    let client_config = input_client_config(&client_identity, server_identity.spki()).unwrap();
    let server_config = input_server_config(&server_identity, client_identity.spki()).unwrap();

    // Keep the peer's advertised stream and connection credit tiny. Once the
    // server stops reading, the typed sender must block and enforce its own
    // safety deadline.
    let mut server_quinn_config = server_config.quinn_config();
    let mut server_transport = quinn::TransportConfig::default();
    server_transport
        .max_concurrent_bidi_streams(1_u8.into())
        .max_concurrent_uni_streams(0_u8.into())
        .stream_receive_window(256_u32.into())
        .receive_window(256_u32.into());
    server_quinn_config.transport = Arc::new(server_transport);

    let server_endpoint = quinn::Endpoint::server(server_quinn_config, LOOPBACK).unwrap();
    let server_address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let accept_endpoint = server_endpoint.clone();
    let accept_config = server_config.clone();
    let accept = tokio::spawn(async move {
        let incoming = accept_endpoint.accept().await.unwrap();
        accept_input(incoming, &accept_config).await.unwrap()
    });

    let mut client = connect_input(&client_endpoint, server_address, &client_config)
        .await
        .unwrap();
    let _server = accept.await.unwrap();

    let mut timed_out = None;
    for sequence in 1..=32 {
        let control = ReliableControlMessage {
            session: session(),
            sequence: ControlSequence(sequence),
            payload: ReliableControl::Enter,
        };
        let started = Instant::now();
        match client.channels_mut().0.send_control(&control).await {
            Ok(()) => {}
            Err(TransportError::ControlWriteTimedOut) => {
                timed_out = Some(started.elapsed());
                break;
            }
            Err(error) => panic!("control write failed before its deadline: {error}"),
        }
    }

    let elapsed = timed_out.expect("flow-control pressure never blocked the control sender");
    assert!(elapsed >= Duration::from_millis(75), "elapsed: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(500), "elapsed: {elapsed:?}");
    tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .expect("timed-out critical write did not close the connection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_server_spki_pin_rejects_the_handshake() {
    let (_client_directory, client_identity) = identity();
    let (_server_directory, server_identity) = identity();
    let (_impostor_directory, impostor_identity) = identity();
    let client_config = input_client_config(&client_identity, impostor_identity.spki()).unwrap();
    let server_config = input_server_config(&server_identity, client_identity.spki()).unwrap();

    let server_endpoint = quinn::Endpoint::server(server_config.quinn_config(), LOOPBACK).unwrap();
    let server_address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let accept_endpoint = server_endpoint.clone();
    let accept_config = server_config.clone();
    let accept = tokio::spawn(async move {
        let incoming = accept_endpoint.accept().await.unwrap();
        accept_input(incoming, &accept_config).await
    });

    assert!(
        connect_input(&client_endpoint, server_address, &client_config)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), accept)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_client_spki_pin_is_rejected_by_the_server() {
    let (_client_directory, client_identity) = identity();
    let (_server_directory, server_identity) = identity();
    let (_impostor_directory, impostor_identity) = identity();
    let client_config = input_client_config(&client_identity, server_identity.spki()).unwrap();
    let server_config = input_server_config(&server_identity, impostor_identity.spki()).unwrap();

    let server_endpoint = quinn::Endpoint::server(server_config.quinn_config(), LOOPBACK).unwrap();
    let server_address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let accept_endpoint = server_endpoint.clone();
    let accept_config = server_config.clone();
    let accept = tokio::spawn(async move {
        let incoming = accept_endpoint.accept().await.unwrap();
        accept_input(incoming, &accept_config).await
    });

    let client = connect_input(&client_endpoint, server_address, &client_config).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), accept)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    if let Ok(client) = client {
        tokio::time::timeout(Duration::from_secs(2), client.closed())
            .await
            .expect("client remained open after server rejected its RPK");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_proves_rpk_possession_and_exports_the_same_transcript_binding() {
    let (_client_directory, client_identity) = identity();
    let (_server_directory, server_identity) = identity();
    let client_config = pairing_client_config(&client_identity).unwrap();
    let server_config = pairing_server_config(&server_identity).unwrap();

    let server_endpoint = quinn::Endpoint::server(server_config.quinn_config(), LOOPBACK).unwrap();
    let server_address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let accept_endpoint = server_endpoint.clone();
    let accept_config = server_config.clone();
    let accept = tokio::spawn(async move {
        let incoming = accept_endpoint.accept().await.unwrap();
        accept_pairing(incoming, &accept_config).await.unwrap()
    });

    let mut client = connect_pairing(&client_endpoint, server_address, &client_config)
        .await
        .unwrap();
    let mut server = accept.await.unwrap();
    assert_eq!(client.peer_spki(), server_identity.spki());
    assert_eq!(server.peer_spki(), client_identity.spki());
    assert_eq!(
        client.transcript_binding().unwrap(),
        server.transcript_binding().unwrap()
    );
    let client_offer = PairingOffer {
        handshake_nonce: [0x11; 32],
        method: PairingMethod::ShortAuthenticationString,
        device_label: Some("client".into()),
        input_port: 43119,
        input_candidates: vec!["127.0.0.1:43119".into()],
    };
    let server_offer = PairingOffer {
        handshake_nonce: [0x22; 32],
        method: PairingMethod::ShortAuthenticationString,
        device_label: Some("server".into()),
        input_port: 43120,
        input_candidates: vec!["127.0.0.1:43120".into()],
    };
    let (seen_by_client, seen_by_server) = tokio::join!(
        client.exchange_offer(&client_offer),
        server.exchange_offer(&server_offer)
    );
    assert_eq!(seen_by_client.unwrap(), server_offer);
    assert_eq!(seen_by_server.unwrap(), client_offer);
    client.close();
    server.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_either_half_of_the_critical_stream_closes_the_connection() {
    let (_client_directory, client_identity) = identity();
    let (_server_directory, server_identity) = identity();
    let client_config = input_client_config(&client_identity, server_identity.spki()).unwrap();
    let server_config = input_server_config(&server_identity, client_identity.spki()).unwrap();

    let server_endpoint = quinn::Endpoint::server(server_config.quinn_config(), LOOPBACK).unwrap();
    let server_address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client(LOOPBACK).unwrap();
    let accept_endpoint = server_endpoint.clone();
    let accept_config = server_config.clone();
    let accept = tokio::spawn(async move {
        let incoming = accept_endpoint.accept().await.unwrap();
        accept_input(incoming, &accept_config).await.unwrap()
    });

    let client = connect_input(&client_endpoint, server_address, &client_config)
        .await
        .unwrap();
    let server = accept.await.unwrap();
    let server_channels = server.into_channels();
    drop(server_channels.control_receive);

    tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .expect("critical stream STOP did not close the connection");
}
