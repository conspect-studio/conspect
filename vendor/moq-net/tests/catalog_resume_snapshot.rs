//! A long-lived open group (a catalog: snapshot at frame 0, deltas after) relayed to a
//! fresh reader after a mid-group resume created the relay's upstream subscription.

mod support;

use std::time::Duration;

use moq_net::{Hop, Timestamp, Version, track};
use support::harness::{MockConnectOptions, connect_mock};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the fresh reader may wait for the snapshot, in virtual time.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(5);

const VERSIONS: &[&str] = &[
	"moq-lite-05",
	"moq-lite-06",
	"moq-lite-07-wip",
	"moq-transport-17",
	"moq-transport-22",
];

fn produce_origin(hop: u64) -> moq_net::origin::Producer {
	let (producer, driver) = moq_net::origin::Producer::new(moq_net::origin::Config::new(Hop::new(hop).unwrap()));
	tokio::spawn(support::harness::run(driver));
	producer
}

/// Returns `Ok(first payload of the group the fresh reader got)` or `Err(reason)`.
async fn scenario(version: &str, resume: Resume, peer_reads: bool) -> Result<(u64, Vec<u8>), String> {
	let publisher = produce_origin(1);
	let relay = produce_origin(2);

	let broadcast = publisher.create_broadcast("demo").unwrap();
	let track = broadcast.create_track("catalog.json", None).unwrap();
	broadcast.announce(Default::default()).unwrap();

	let ts = |ms| Timestamp::from_millis(ms).unwrap();
	// The catalog: one group that never ends, a snapshot then deltas.
	let mut open = track.append_group().unwrap();
	open.write_frame(ts(0), b"snapshot".as_ref()).unwrap();
	open.write_frame(ts(10), b"delta1".as_ref()).unwrap();
	open.write_frame(ts(20), b"delta2".as_ref()).unwrap();
	let sequence = open.sequence;

	let version: Version = version.parse().unwrap();
	// The relay pulls from the publisher; a mesh peer pulls from the relay.
	let mut options = MockConnectOptions::new(version);
	options.server_publish = Some(publisher.consume());
	options.client_subscribe = Some(relay.clone());
	let _upstream = connect_mock(options).await;

	let consumer = relay.consume();
	consumer.routed("demo").await.unwrap();
	let remote = consumer.request_broadcast("demo").await.unwrap();

	// A frame-precise resume on the relay itself, as a downstream peer without the head
	// widening sends on the wire: the relay's own upstream SUBSCRIBE must widen it.
	let _local = if let Resume::Local(resume_frame) = resume {
		let start = track::Position {
			group: sequence,
			frame: resume_frame,
		};
		let mut sub = remote
			.track("catalog.json")
			.unwrap()
			.subscribe(track::Subscription::default().with_start(start))
			.await
			.unwrap();
		let group = sub.recv_group().await.unwrap().unwrap();
		Some((sub, group))
	} else {
		None
	};

	// A mesh peer resumes the catalog mid-group over a session: it already holds frame 0.
	let _resumed = if let Resume::Peer(resume_frame) = resume {
		let peer = produce_origin(3);
		let mut options = MockConnectOptions::new(version);
		options.server_publish = Some(relay.consume());
		options.client_subscribe = Some(peer.clone());
		let link = connect_mock(options).await;

		let peer_consumer = peer.consume();
		peer_consumer.routed("demo").await.unwrap();
		let peer_remote = peer_consumer.request_broadcast("demo").await.unwrap();
		let start = track::Position {
			group: sequence,
			frame: resume_frame,
		};
		let mut sub = peer_remote
			.track("catalog.json")
			.unwrap()
			.subscribe(track::Subscription::default().with_start(start))
			.await
			.unwrap();
		// The peer only forwards demand, as a relay serving its own resumed subscriber
		// does: reading its copy here would make it fetch the head through the relay.
		let mut group = sub.recv_group().await.unwrap().unwrap();
		if peer_reads {
			// A local group reader starts at the head, so the peer's own copy must be able
			// to serve it too, not just the relay's.
			let read = tokio::time::timeout(Duration::from_millis(500), group.read_frame()).await;
			match read {
				Ok(Ok(Some(frame))) if frame.payload.as_ref() == b"snapshot" => {}
				other => return Err(format!("peer's first read was {other:?}, not the snapshot")),
			}
		}
		Some((link, peer, sub, group))
	} else {
		None
	};

	// A brand-new reader wants the latest group from its snapshot.
	let fresh = async {
		let mut sub = remote.track("catalog.json").unwrap().subscribe(None).await.unwrap();
		let mut group = sub.recv_group().await.unwrap().unwrap();
		let frame = group.read_frame().await;
		(group.sequence, frame)
	};
	match tokio::time::timeout(SNAPSHOT_WAIT, fresh).await {
		Err(_) => Err("fresh reader received nothing (timed out)".into()),
		Ok((seq, Ok(Some(frame)))) => Ok((seq, frame.payload.to_vec())),
		Ok((seq, Ok(None))) => Err(format!("group {seq} ended empty")),
		Ok((seq, Err(err))) => Err(format!("group {seq} errored: {err}")),
	}
}

/// Where the mid-group resume comes from, if any.
#[derive(Clone, Copy)]
enum Resume {
	None,
	/// A mesh peer over a session, which widens its own upstream start.
	Peer(u64),
	/// A subscriber on the relay itself, so only the relay's widening can help.
	Local(u64),
}

async fn run(resume: Resume, peer_reads: bool) {
	let mut failures = Vec::new();
	for version in VERSIONS {
		let result = tokio::time::timeout(TEST_TIMEOUT, scenario(version, resume, peer_reads))
			.await
			.unwrap_or_else(|_| Err("scenario timed out".into()));
		match result {
			Ok((0, payload)) if payload == b"snapshot" => {}
			Ok((seq, payload)) => failures.push(format!(
				"{version}: got group {seq} starting with {:?}",
				String::from_utf8_lossy(&payload)
			)),
			Err(err) => failures.push(format!("{version}: {err}")),
		}
	}
	assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// The regression: a resume at (G, 1) must not keep a fresh reader from G's snapshot.
#[tokio::test(start_paused = true)]
async fn fresh_reader_gets_snapshot_after_mid_group_resume() {
	run(Resume::Peer(1), false).await;
}

/// The same resume arriving frame-precise at the relay, as from a not-yet-upgraded peer.
#[tokio::test(start_paused = true)]
async fn fresh_reader_gets_snapshot_after_local_mid_group_resume() {
	run(Resume::Local(1), false).await;
}

/// Control: with no prior resume the fresh reader gets the snapshot.
#[tokio::test(start_paused = true)]
async fn fresh_reader_gets_snapshot_without_resume() {
	run(Resume::None, false).await;
}

/// Control: the same resume from the head of the group, (G, 0).
#[tokio::test(start_paused = true)]
async fn fresh_reader_gets_snapshot_after_head_resume() {
	run(Resume::Peer(0), false).await;
}

/// Variant: the peer also reads its own copy from the head before the fresh reader
/// arrives, so both the peer's copy and the relay's must hold frame 0.
#[tokio::test(start_paused = true)]
async fn fresh_reader_gets_snapshot_after_peer_fetches_head() {
	run(Resume::Peer(1), true).await;
}
