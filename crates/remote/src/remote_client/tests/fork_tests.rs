//! The fork's own tests of the reconnect path: the shared connection, the
//! backoff, stuck peers, and a server that has gone.

use super::*;

/// 2026-10-15: one window on the box whose handshake kept timing out killed
/// the SSH connection the other three had just reconnected over, on every
/// one of its attempts.
#[gpui::test]
async fn test_a_stuck_reconnect_leaves_the_shared_connection_alone(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (options, _, connect_guard) = MockConnection::new(cx, server_cx);
    drop(connect_guard);
    let mock = cx
        .update(|cx| {
            cx.default_global::<MockConnectionRegistry>()
                .take(&options)
                .unwrap()
        })
        .await;
    let (proxy_tx, mut proxy_rx) = mpsc::unbounded();
    *mock.proxy_tx.lock() = Some(proxy_tx);
    let kills = mock.clone();
    let connection: Arc<dyn RemoteConnection> = mock;
    cx.update(|cx| {
        release_channel::init_test(Version::new(0, 0, 0), ReleaseChannel::Dev, cx);
        cx.default_global::<ConnectionPool>().connections.insert(
            connection.connection_options(),
            ConnectionPoolEntry::Connected(Arc::downgrade(&connection)),
        );
    });

    let mut connect = async |cx: &mut TestAppContext| {
        let (_cancellation_tx, cancellation_rx) = oneshot::channel();
        let client = cx.update(|cx| {
            RemoteClient::new(
                ConnectionIdentifier::setup(),
                connection.clone(),
                cancellation_rx,
                Arc::new(MockDelegate),
                cx,
            )
        });
        let (incoming_tx, mut outgoing_rx) = proxy_rx.next().await.unwrap();
        outgoing_rx.next().await.unwrap();
        incoming_tx
            .unbounded_send(proto::RemoteStarted {}.into_envelope(0, None, None))
            .unwrap();
        outgoing_rx.next().await.unwrap();
        let ping = outgoing_rx.next().await.unwrap();
        let mut ping_response = proto::Ack {}.into_envelope(2, Some(ping.id), None);
        ping_response.ack_id = Some(ping.id);
        incoming_tx.unbounded_send(ping_response).unwrap();
        let client = client.await.unwrap().unwrap();
        (client, incoming_tx, outgoing_rx)
    };
    let (stuck, _stuck_in, _stuck_out) = connect(cx).await;
    let (healthy, healthy_in, mut healthy_out) = connect(cx).await;
    // The healthy window's server answers its heartbeats.
    let _healthy_server = cx.background_spawn(async move {
        let mut next_id = 3;
        while let Some(message) = healthy_out.next().await {
            if matches!(message.payload, Some(proto::envelope::Payload::Ping(_))) {
                let mut ack = proto::Ack {}.into_envelope(next_id, Some(message.id), None);
                ack.ack_id = Some(message.id);
                healthy_in.unbounded_send(ack).ok();
                next_id += 1;
            }
        }
    });

    stuck.update(cx, |stuck, cx| stuck.reconnect(cx).unwrap());
    // Its new proxy starts, and nothing ever answers its handshake.
    let _unanswered = proxy_rx.next().await.unwrap();
    cx.run_until_parked();
    assert_eq!(
        kills.kill_count(),
        0,
        "another client is connected over it, so it stays up"
    );

    for _ in 0..MAX_RECONNECT_ATTEMPTS + 1 {
        cx.background_executor
            .timer(RECONNECT_HANDSHAKE_TIMEOUT + QUIET_RECONNECT_INTERVAL)
            .await;
        cx.run_until_parked();
        while proxy_rx.next().now_or_never().flatten().is_some() {}
    }
    assert_eq!(kills.kill_count(), 0);
    assert_eq!(
        healthy.read_with(cx, |healthy, _| healthy.connection_state()),
        ConnectionState::Connected,
        "the healthy window stays connected through every attempt"
    );
    assert_eq!(
        stuck.read_with(cx, |stuck, _| stuck.connection_state()),
        ConnectionState::Reconnecting,
        "and the stuck one keeps trying rather than giving up on a timeout"
    );
}

/// 2026-10-21: a window whose SSH master had exited retried for hours
/// over that dead connection, because other windows on the host were
/// connected (over another one).
#[gpui::test]
async fn test_a_client_whose_connection_died_gets_a_new_one(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (options, _, connect_guard) = MockConnection::new(cx, server_cx);
    drop(connect_guard);
    let take = async |cx: &mut TestAppContext| {
        cx.update(|cx| {
            cx.default_global::<MockConnectionRegistry>()
                .take(&options)
                .unwrap()
        })
        .await
    };
    let dying = take(cx).await;
    let RemoteConnectionOptions::Mock(mock_options) = dying.connection_options() else {
        unreachable!()
    };
    let (_, connect_guard) = MockConnection::new_with_opts(mock_options.clone(), cx, server_cx);
    drop(connect_guard);
    let live = take(cx).await;
    let mut proxies = Vec::new();
    for mock in [&dying, &live] {
        let (proxy_tx, proxy_rx) = mpsc::unbounded();
        *mock.proxy_tx.lock() = Some(proxy_tx);
        proxies.push(proxy_rx);
    }
    let mut live_proxy = proxies.pop().unwrap();
    let mut dying_proxy = proxies.pop().unwrap();
    let dying_connection: Arc<dyn RemoteConnection> = dying.clone();
    let live_connection: Arc<dyn RemoteConnection> = live.clone();
    cx.update(|cx| {
        release_channel::init_test(Version::new(0, 0, 0), ReleaseChannel::Dev, cx);
        // The pool still hands out the connection that is about to die.
        cx.default_global::<ConnectionPool>().connections.insert(
            dying_connection.connection_options(),
            ConnectionPoolEntry::Connected(Arc::downgrade(&dying_connection)),
        );
    });

    let connect = async |connection: &Arc<dyn RemoteConnection>,
                         proxy: &mut mpsc::UnboundedReceiver<_>,
                         cx: &mut TestAppContext| {
        let (_cancellation_tx, cancellation_rx) = oneshot::channel();
        let client = cx.update(|cx| {
            RemoteClient::new(
                ConnectionIdentifier::setup(),
                connection.clone(),
                cancellation_rx,
                Arc::new(MockDelegate),
                cx,
            )
        });
        let (incoming_tx, mut outgoing_rx): (
            mpsc::UnboundedSender<Envelope>,
            mpsc::UnboundedReceiver<Envelope>,
        ) = proxy.next().await.unwrap();
        outgoing_rx.next().await.unwrap();
        incoming_tx
            .unbounded_send(proto::RemoteStarted {}.into_envelope(0, None, None))
            .unwrap();
        outgoing_rx.next().await.unwrap();
        let ping = outgoing_rx.next().await.unwrap();
        let mut ping_response = proto::Ack {}.into_envelope(2, Some(ping.id), None);
        ping_response.ack_id = Some(ping.id);
        incoming_tx.unbounded_send(ping_response).unwrap();
        let client = client.await.unwrap().unwrap();
        (client, incoming_tx, outgoing_rx)
    };
    let (stuck, _stuck_in, _stuck_out) = connect(&dying_connection, &mut dying_proxy, cx).await;
    let (peer, peer_in, mut peer_out) = connect(&live_connection, &mut live_proxy, cx).await;
    let _peer_server = cx.background_spawn(async move {
        let mut next_id = 3;
        while let Some(message) = peer_out.next().await {
            if matches!(message.payload, Some(proto::envelope::Payload::Ping(_))) {
                let mut ack = proto::Ack {}.into_envelope(next_id, Some(message.id), None);
                ack.ack_id = Some(message.id);
                peer_in.unbounded_send(ack).ok();
                next_id += 1;
            }
        }
    });

    // The new connection the next attempt should get.
    let (_, connect_guard) = MockConnection::new_with_opts(mock_options.clone(), cx, server_cx);
    drop(connect_guard);
    let fresh = cx.update(|cx| {
        cx.default_global::<MockConnectionRegistry>()
            .peek(&mock_options)
            .unwrap()
    });
    let (proxy_tx, mut fresh_proxy) = mpsc::unbounded();
    *fresh.proxy_tx.lock() = Some(proxy_tx);

    dying.simulate_master_exit();
    stuck.update(cx, |stuck, cx| stuck.reconnect(cx).unwrap());
    cx.run_until_parked();
    assert!(
        fresh_proxy.next().now_or_never().flatten().is_some(),
        "the attempt runs over a new connection"
    );
    assert!(dying_proxy.next().now_or_never().flatten().is_none());
    assert_eq!(
        dying.kill_count(),
        1,
        "nobody else uses the dead one, so it is cleaned up"
    );
    assert_eq!(live.kill_count(), 0, "the peer's connection is left alone");
    assert_eq!(
        peer.read_with(cx, |peer, _| peer.connection_state()),
        ConnectionState::Connected
    );
}

/// A server that answers a client's handshakes, flushes and heartbeats.
fn answer_client(
    incoming_tx: mpsc::UnboundedSender<Envelope>,
    mut outgoing_rx: mpsc::UnboundedReceiver<Envelope>,
    cx: &TestAppContext,
) -> Task<()> {
    cx.background_spawn(async move {
        let mut next_id = 1000;
        while let Some(message) = outgoing_rx.next().await {
            if matches!(
                message.payload,
                Some(
                    proto::envelope::Payload::RemoteStarted(_)
                        | proto::envelope::Payload::FlushBufferedMessages(_)
                        | proto::envelope::Payload::Ping(_)
                )
            ) {
                let mut ack = proto::Ack {}.into_envelope(next_id, Some(message.id), None);
                ack.ack_id = Some(message.id);
                incoming_tx.unbounded_send(ack).ok();
                next_id += 1;
            }
        }
    })
}

/// 2026-10-23: one window reconnecting retried the others on the host from
/// inside its own update, and their reconnects read it back: Zed panicked
/// with "cannot read RemoteClient while it is already being updated".
#[gpui::test]
async fn test_a_reconnect_wakes_a_stuck_peer_without_panicking(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (options, _, connect_guard) = MockConnection::new(cx, server_cx);
    drop(connect_guard);
    let mock = cx
        .update(|cx| {
            cx.default_global::<MockConnectionRegistry>()
                .take(&options)
                .unwrap()
        })
        .await;
    let (proxy_tx, mut proxy_rx) = mpsc::unbounded();
    *mock.proxy_tx.lock() = Some(proxy_tx);
    let connection: Arc<dyn RemoteConnection> = mock;
    cx.update(|cx| {
        release_channel::init_test(Version::new(0, 0, 0), ReleaseChannel::Dev, cx);
        cx.default_global::<ConnectionPool>().connections.insert(
            connection.connection_options(),
            ConnectionPoolEntry::Connected(Arc::downgrade(&connection)),
        );
    });

    let mut connect = async |cx: &mut TestAppContext| {
        let (_cancellation_tx, cancellation_rx) = oneshot::channel();
        let client = cx.update(|cx| {
            RemoteClient::new(
                ConnectionIdentifier::setup(),
                connection.clone(),
                cancellation_rx,
                Arc::new(MockDelegate),
                cx,
            )
        });
        let (incoming_tx, mut outgoing_rx) = proxy_rx.next().await.unwrap();
        outgoing_rx.next().await.unwrap();
        incoming_tx
            .unbounded_send(proto::RemoteStarted {}.into_envelope(0, None, None))
            .unwrap();
        outgoing_rx.next().await.unwrap();
        let ping = outgoing_rx.next().await.unwrap();
        let mut ping_response = proto::Ack {}.into_envelope(2, Some(ping.id), None);
        ping_response.ack_id = Some(ping.id);
        incoming_tx.unbounded_send(ping_response).unwrap();
        client.await.unwrap().unwrap()
    };
    let first = connect(cx).await;
    let second = connect(cx).await;

    // The second window's attempt goes unanswered and fails, leaving it
    // waiting out its backoff.
    second.update(cx, |second, cx| second.reconnect(cx).unwrap());
    let _unanswered = proxy_rx.next().await.unwrap();
    cx.background_executor
        .timer(RECONNECT_HANDSHAKE_TIMEOUT + PROXY_EXIT_GRACE)
        .await;
    cx.run_until_parked();
    assert!(second.read_with(cx, |second, _| second.state_is(State::is_reconnect_failed)));

    // The first window gets through, which retries the second at once.
    first.update(cx, |first, cx| first.reconnect(cx).unwrap());
    let (incoming_tx, outgoing_rx) = proxy_rx.next().await.unwrap();
    let _first_server = answer_client(incoming_tx, outgoing_rx, cx);
    cx.run_until_parked();
    assert_eq!(
        first.read_with(cx, |first, _| first.connection_state()),
        ConnectionState::Connected
    );
    let (incoming_tx, outgoing_rx) = proxy_rx
        .next()
        .now_or_never()
        .flatten()
        .expect("the stuck window retried without waiting out its backoff");
    let _second_server = answer_client(incoming_tx, outgoing_rx, cx);
    cx.run_until_parked();
    assert_eq!(
        second.read_with(cx, |second, _| second.connection_state()),
        ConnectionState::Connected
    );
}

/// 2026-10-23: overnight every box window's server exited, as it does after
/// two idle hours, and in the morning each window retried forever: the
/// failed handshake ended the attempt before the proxy's exit code (90,
/// no server running) was read, so a fresh server was never started.
#[gpui::test]
async fn test_a_reconnect_that_finds_no_server_stops_at_once(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (options, _, connect_guard) = MockConnection::new(cx, server_cx);
    drop(connect_guard);
    let mock = cx
        .update(|cx| {
            cx.default_global::<MockConnectionRegistry>()
                .take(&options)
                .unwrap()
        })
        .await;
    let (proxy_tx, mut proxy_rx) = mpsc::unbounded();
    *mock.proxy_tx.lock() = Some(proxy_tx);
    let proxies = mock.clone();
    let connection: Arc<dyn RemoteConnection> = mock;
    let (_cancellation_tx, cancellation_rx) = oneshot::channel();
    let client = cx.update(|cx| {
        release_channel::init_test(Version::new(0, 0, 0), ReleaseChannel::Dev, cx);
        cx.default_global::<ConnectionPool>().connections.insert(
            connection.connection_options(),
            ConnectionPoolEntry::Connected(Arc::downgrade(&connection)),
        );
        RemoteClient::new(
            ConnectionIdentifier::setup(),
            connection.clone(),
            cancellation_rx,
            Arc::new(MockDelegate),
            cx,
        )
    });
    let (incoming_tx, mut outgoing_rx) = proxy_rx.next().await.unwrap();
    outgoing_rx.next().await.unwrap();
    incoming_tx
        .unbounded_send(proto::RemoteStarted {}.into_envelope(0, None, None))
        .unwrap();
    outgoing_rx.next().await.unwrap();
    let ping = outgoing_rx.next().await.unwrap();
    let mut ping_response = proto::Ack {}.into_envelope(2, Some(ping.id), None);
    ping_response.ack_id = Some(ping.id);
    incoming_tx.unbounded_send(ping_response).unwrap();
    let client = client.await.unwrap().unwrap();
    let disconnects = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let disconnects = disconnects.clone();
        cx.subscribe(&client, move |_, event, _| {
            if let RemoteClientEvent::Disconnected { server_not_running } = event {
                disconnects.borrow_mut().push(*server_not_running);
            }
        })
    });

    *proxies.proxy_exit_code.lock() = Some(ProxyLaunchError::ServerNotRunning.to_exit_code());
    client.update(cx, |client, cx| client.reconnect(cx).unwrap());
    // The proxy's output closes first, failing the handshake, and its
    // exit status follows.
    let (incoming_tx, _outgoing_rx) = proxy_rx.next().await.unwrap();
    drop(incoming_tx);
    cx.run_until_parked();
    cx.background_executor.timer(Duration::from_secs(1)).await;
    cx.run_until_parked();

    assert!(client.read_with(cx, |client, _| {
        client.state_is(State::is_server_not_running)
    }));
    assert_eq!(*disconnects.borrow(), [true]);
    cx.background_executor
        .timer(QUIET_RECONNECT_INTERVAL * 2)
        .await;
    cx.run_until_parked();
    assert!(
        proxy_rx.next().now_or_never().flatten().is_none(),
        "one attempt, and no retries"
    );
}

/// 2026-10-19: a connection after a restart took `setup-1` and killed the
/// server the previous launch's `setup-1` was still running.
#[gpui::test]
fn test_setup_identifiers_differ_between_launches(cx: &mut TestAppContext) {
    cx.update(|cx| release_channel::init_test(Version::new(0, 0, 0), ReleaseChannel::Stable, cx));
    let name = cx.update(|cx| ConnectionIdentifier::Setup(1).to_string(cx));
    assert_eq!(name, format!("setup-{}-1", *LAUNCH_TAG));
    assert_eq!(launch_tag(0), "00000");
    assert_eq!(launch_tag(1_792_000_000), launch_tag(1_792_000_000));
    assert_ne!(launch_tag(1_792_000_000), launch_tag(1_792_000_001));
    assert!(name.len() <= 20, "{name} must fit in a socket path");
    let reused = cx.update(|cx| ConnectionIdentifier::Reused("setup-x-4".into()).to_string(cx));
    assert_eq!(reused, "setup-x-4");
}

#[test]
fn test_only_failures_retrying_cannot_fix_end_the_attempts() {
    assert!(is_unrecoverable_reconnect_error(&anyhow!(
        "ssh: Permission denied (publickey)"
    )));
    assert!(is_unrecoverable_reconnect_error(&anyhow!(
        "Host key verification failed."
    )));
    assert!(!is_unrecoverable_reconnect_error(&anyhow!(
        "ssh: connect to host box port 22: Network is unreachable"
    )));
    assert_eq!(reconnect_delay(1), Duration::ZERO);
    assert_eq!(reconnect_delay(2), Duration::from_secs(1));
    assert_eq!(reconnect_delay(4), Duration::from_secs(5));
    assert_eq!(reconnect_delay(9), QUIET_RECONNECT_INTERVAL);
}
