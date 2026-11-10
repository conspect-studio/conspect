use std::task::{Context, Poll};

use super::{Message, Version};
use crate::{
	Error,
	coding::{Decode, DecodeError, Stream},
};

/// Legacy receives are framed once by the physical multiplexer, not again by
/// the virtual request decoder. Encodes still count at their typed codec.
pub(super) fn with_stats<S: crate::transport::poll::Session>(
	stream: Stream<S, Version>,
	stats: &crate::stats::Session,
	version: Version,
) -> Stream<S, Version> {
	let mut stream = stream.with_stats(stats);
	if matches!(version, Version::Draft14 | Version::Draft15 | Version::Draft16) {
		stream.reader = stream.reader.with_stats(&Default::default());
	}
	stream
}

/// Whether the requester's FIN cancels its request; later drafts only end its updates.
pub(super) fn fin_cancels(version: Version) -> bool {
	matches!(
		version,
		Version::Draft14 | Version::Draft15 | Version::Draft16 | Version::Draft17 | Version::Draft18
	)
}

/// Watch abrupt cancellation after the requester has finished sending messages.
pub(super) fn poll_cancel<S: crate::transport::poll::Session>(
	stream: &mut Stream<S, Version>,
	finished: &mut bool,
	version: Version,
	cx: &mut Context<'_>,
) -> Poll<Result<(), Error>> {
	if !*finished {
		match stream.reader.poll_closed(cx) {
			Poll::Ready(Ok(())) if !fin_cancels(version) => *finished = true,
			Poll::Ready(result) => return Poll::Ready(result),
			Poll::Pending => {}
		}
	}
	// Virtual writers on drafts 14-16 are already closed while their control request lives.
	if !matches!(version, Version::Draft14 | Version::Draft15 | Version::Draft16) {
		return stream.writer.poll_closed(cx);
	}
	Poll::Pending
}

/// A framed draft-17+ REQUEST_UPDATE on a subscription, preserving omitted preferences.
#[derive(Debug)]
pub(super) struct Update {
	pub(super) priority: Option<u8>,
	/// It asks for something other than a priority change, which we cannot apply.
	pub(super) unsupported: bool,
	/// The encoded size of the type selector, which the sender writes apart from the message.
	selector: usize,
}

impl Decode<Version> for Update {
	fn decode<R: bytes::Buf>(r: &mut R, version: Version) -> Result<Self, DecodeError> {
		let start = r.remaining();
		if u64::decode(r, version)? != super::SubscribeUpdate::ID {
			return Err(DecodeError::InvalidValue);
		}
		let selector = start - r.remaining();
		let size = u16::decode(r, version)? as usize;
		if r.remaining() < size {
			return Err(DecodeError::Short);
		}
		let mut data = r.copy_to_bytes(size);
		// The complete frame is present; a short field inside it is malformed.
		let update = Self::decode_body(&mut data, version).map_err(DecodeError::complete)?;
		Ok(Self { selector, ..update })
	}

	fn control(&self, version: Version) -> Option<crate::stats::Control> {
		Some(super::message::control(super::SubscribeUpdate::ID, version))
	}

	fn uncounted(&self) -> usize {
		self.selector
	}
}

impl Update {
	fn decode_body(data: &mut bytes::Bytes, version: Version) -> Result<Self, DecodeError> {
		use super::{Fill, Filter, Opaque, RequestId};
		use bytes::Buf;

		RequestId::decode(data, version)?;
		if version == Version::Draft17 {
			// Required Request ID Delta, removed in draft 18.
			u64::decode(data, version)?;
		}
		decode_params!(data, version,
			0x02 => object_timeout: Option<u64>,
			0x03 => token: Vec<Opaque>,
			0x06 => subgroup_timeout: Option<u64>,
			0x10 => forward: Option<bool>,
			0x20 => priority: Option<u8>,
			0x21 => filter: Option<Filter>,
			0x23 => fill: Option<Fill>,
			0x25 => subgroup_filter: Vec<Opaque>,
			0x26 => object_filter: Vec<Opaque>,
			0x27 => priority_filter: Vec<Opaque>,
			0x28 => property_filter: Vec<Opaque>,
			0x29 => track_filter: Vec<Opaque>,
			0x32 => new_group: Option<u64>,
		);
		if data.has_remaining() {
			return Err(DecodeError::Long);
		}
		Ok(Self {
			selector: 0,
			priority,
			unsupported: forward == Some(false)
				|| filter.is_some()
				|| fill.is_some()
				|| object_timeout.is_some()
				|| subgroup_timeout.is_some()
				|| !token.is_empty()
				|| !subgroup_filter.is_empty()
				|| !object_filter.is_empty()
				|| !priority_filter.is_empty()
				|| !property_filter.is_empty()
				|| !track_filter.is_empty()
				|| new_group.is_some(),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A request-stream message counts as its sender counts it: the length and body, not the
	/// type that selects it.
	#[tokio::test]
	async fn request_stream_messages_count_without_their_type() {
		use crate::lite::test_transport::ScriptedSession;
		use crate::stats::{Control, Registry, Tier};
		let registry = Registry::new(Default::default());
		let stats = registry.tier(Tier::default()).session("");
		let version = Version::Draft22;
		// GOAWAY, REQUEST_OK, then a REQUEST_UPDATE with no parameters.
		let mut session = ScriptedSession::new(vec![0x10, 0, 2, 0, 0, 0x07, 0, 0, 0x02, 0, 2, 0, 0]);
		let mut stream = with_stats(Stream::open(&mut session, version).await.unwrap(), &stats, version);
		for expected in [super::super::GoAway::ID, super::super::RequestOk::ID] {
			let id: u64 = stream.reader.decode().await.unwrap();
			assert_eq!(id, expected);
			stream.reader.read_message(id).await.unwrap();
		}
		stream.reader.decode::<Update>().await.unwrap();
		let rx = |class| {
			let rows = registry.snapshot().control();
			rows.into_iter().find(|row| row.1 == class).map(|row| row.2.rx)
		};
		assert_eq!(rx(Control::Goaway), Some(4));
		assert_eq!(rx(Control::Other), Some(2 + 4));
	}
}
