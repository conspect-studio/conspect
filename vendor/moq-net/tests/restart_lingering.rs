//! A publisher restarting while its old session lingers on the relay.
//!
//! A publisher that dies without a close (SIGKILL, a dropped uplink) leaves its session on
//! the relay until the transport idles it out. The restarted process announces the same
//! path under a new first hop, so the two routes tie on everything but the hash.

mod support;

use std::time::Duration;

use moq_net::{Hop, Timestamp, Version, origin};
use support::harness::{MockConnectOptions, MockPair, connect_mock};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

fn produce_origin(hop: u64) -> origin::Producer {
	let (producer, driver) = origin::Producer::new(origin::Config::new(Hop::new(hop).unwrap()));
	tokio::spawn(support::harness::run(driver));
	producer
}

/// Connect `publisher` to `relay` as a client publishing into it.
async fn link(version: Version, publisher: &origin::Producer, relay: &origin::Producer) -> MockPair {
	let mut options = MockConnectOptions::new(version);
	options.client_publish = Some(publisher.consume());
	options.server_subscribe = Some(relay.clone());
	connect_mock(options).await
}

/// The restart wins at once, whichever chain hashes lower, so a viewer's first request
/// reaches it instead of waiting on the dead session.
async fn restart_while_the_old_session_lingers(version: Version, old_hop: u64, new_hop: u64) {
	let relay = produce_origin(3);
	let mut announced = relay.consume().announced();

	let old = produce_origin(old_hop);
	let old_cam = old.create_broadcast("cam").unwrap();
	let _old_video = old_cam.create_track("video", None).unwrap();
	old_cam.announce(Default::default()).unwrap();
	let old_session = link(version, &old, &relay).await;
	announced.next().await.expect("the old route announces");

	// The old process dies without a close: its session falls silent.
	old_session.client_transport.set_latency(Duration::from_secs(3600));
	old_session.server_transport.set_latency(Duration::from_secs(3600));

	let new = produce_origin(new_hop);
	let new_cam = new.create_broadcast("cam").unwrap();
	let new_video = new_cam.create_track("video", None).unwrap();
	new_cam.announce(Default::default()).unwrap();
	let _new_session = link(version, &new, &relay).await;
	tokio::time::sleep(origin::DEFAULT_UPDATE_HOLD * 2).await;

	let cam = relay.consume().request_broadcast("cam").await.unwrap();
	let mut video = cam.track("video").unwrap().subscribe(None).await.unwrap();
	let mut group = new_video.append_group().unwrap();
	group.write_frame(Timestamp::ZERO, b"new:0".as_ref()).unwrap();
	let mut group = video.recv_group().await.unwrap().expect("a group");
	let frame = group.read_frame().await.unwrap().expect("a frame");
	assert_eq!(frame.payload.as_ref(), b"new:0", "{version} ({old_hop} -> {new_hop})");
}

#[tokio::test(start_paused = true)]
async fn restart_while_the_old_session_lingers_wins() {
	for version in ["moq-lite-06", "moq-lite-07-wip", "moq-transport-22"] {
		for (old_hop, new_hop) in [(1, 4), (4, 1)] {
			let version: Version = version.parse().unwrap();
			tokio::time::timeout(
				TEST_TIMEOUT,
				restart_while_the_old_session_lingers(version, old_hop, new_hop),
			)
			.await
			.unwrap_or_else(|_| panic!("{version} ({old_hop} -> {new_hop}): subscribe hung"));
		}
	}
}
