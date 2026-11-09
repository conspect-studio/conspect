//! Control counters cross real protocol drivers, including legacy multiplexing.
//!
//! Time is paused, so the waits below cost nothing and never touch the wall clock.

mod support;

use std::time::Duration;

use moq_net::{
	Hop, Timestamp, Version, origin,
	stats::{Control, Registry, Tier},
};
use support::harness::{MockConnectOptions, connect_mock, now, run};
use support::mock::create_mock_session_pair;

const TIMEOUT: Duration = Duration::from_secs(10);

fn origin(hop: u64) -> origin::Producer {
	let (origin, driver) = origin::Producer::new(origin::Config::new(Hop::new(hop).unwrap()));
	tokio::spawn(run(driver));
	origin
}

#[tokio::test(start_paused = true)]
async fn control_classes_cross_lite_and_ietf_sessions_without_counting_payload() {
	for version in [
		"moq-lite-03",
		"moq-lite-06",
		"moq-lite-07-wip",
		"moq-transport-14",
		"moq-transport-15",
		"moq-transport-16",
		"moq-transport-22",
	] {
		let publisher = origin(1);
		let subscriber = origin(2);
		let registry = Registry::new(Default::default());
		let tier = Tier::new("internal");
		let mut options = MockConnectOptions::new(version.parse::<Version>().unwrap());
		options.server_stats = registry.tier(tier.clone()).session("");
		options.client_publish = Some(publisher.consume());
		options.server_subscribe = Some(subscriber.clone());
		let _pair = connect_mock(options).await;
		let broadcast = publisher.create_broadcast("room").unwrap();
		let track = broadcast.create_track("video", None).unwrap();
		broadcast.announce(Default::default()).unwrap();
		let consumer = subscriber.consume();
		tokio::time::timeout(TIMEOUT, consumer.routed("room"))
			.await
			.expect("announce timeout")
			.expect("routed");
		let remote = tokio::time::timeout(TIMEOUT, consumer.request_broadcast("room"))
			.await
			.expect("resolve timeout")
			.expect("broadcast resolves");
		let reader = tokio::spawn(async move {
			let mut sub = remote.track("video").unwrap().subscribe(None).await.unwrap();
			let mut group = sub.recv_group().await.unwrap().unwrap();
			group.read_frame().await.unwrap().unwrap().payload.len()
		});
		tokio::time::timeout(TIMEOUT, track.used())
			.await
			.expect("no subscriber appeared")
			.unwrap();
		let mut group = track.append_group().unwrap();
		group
			.write_frame(Timestamp::ZERO, bytes::Bytes::from(vec![7; 64 * 1024]))
			.unwrap();
		group.finish().unwrap();
		let read = tokio::time::timeout(TIMEOUT, reader)
			.await
			.expect("group timeout")
			.unwrap();
		assert_eq!(read, 64 * 1024, "{version}");
		let rows = registry.snapshot().control();
		for class in [Control::Announce, Control::Subscribe] {
			assert!(
				rows.iter()
					.any(|(row_tier, row_class, bytes)| *row_tier == tier && *row_class == class && bytes.tx > 0),
				"{version}: {class:?}: {rows:?}"
			);
		}
		let rx: u64 = rows.iter().map(|(_, _, bytes)| bytes.rx).sum();
		assert!(
			rx > 0 && rx < 64 * 1024,
			"{version}: media payload entered control counters: {rows:?}"
		);
	}
}

/// The SETUP a server reads before admission counts under the stats the handshake is
/// admitted with, even for a session with neither origin.
#[tokio::test(start_paused = true)]
async fn setup_counts_under_the_admitted_stats() {
	for version in ["moq-lite-07-wip", "moq-transport-14", "moq-transport-22"] {
		let version: Version = version.parse().unwrap();
		let registry = Registry::new(Default::default());
		let tier = Tier::new("internal");
		let (client_transport, server_transport) = create_mock_session_pair(Some(version.alpn()));
		let client = moq_net::Client::new().with_versions(version.into());
		let server = moq_net::Server::new().with_versions(version.into());
		let stats = registry.tier(tier.clone()).session("");
		let client = async {
			let (session, driver) = client.connect(now(), client_transport).await.unwrap();
			tokio::spawn(run(driver));
			session
		};
		let server = async {
			let request = server.accept_request(now(), server_transport).await.unwrap();
			let (session, driver) = request.with_stats(stats).ok().await.unwrap();
			tokio::spawn(run(driver));
			session
		};
		let _sessions = tokio::join!(client, server);
		let counted = || {
			registry
				.snapshot()
				.control()
				.into_iter()
				.any(|(row_tier, class, bytes)| {
					row_tier == tier && class == Control::Session && bytes.tx > 0 && bytes.rx > 0
				})
		};
		tokio::time::timeout(TIMEOUT, async {
			while !counted() {
				tokio::time::sleep(Duration::from_millis(1)).await;
			}
		})
		.await
		.unwrap_or_else(|_| panic!("{version}: {:?}", registry.snapshot().control()));
	}
}
