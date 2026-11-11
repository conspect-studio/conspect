#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use super::*;
use multiview_config::{GraphicsDataSource, GraphicsItem, WeatherLocation};

fn config() -> GraphicsConfig {
    let mut c = GraphicsConfig::default();
    c.data_sources
        .push(GraphicsDataSource::from_weather_location(
            &WeatherLocation::new("sydney", "Sydney", "IDN11060", "NSW_PT131"),
        ));
    for id in ["a", "b", "c"] {
        c.items.push(GraphicsItem::new(
            id,
            id.to_uppercase(),
            "weather.today",
            Some("sydney".into()),
        ));
    }
    let mut ch = GraphicsChannel::new(DEFAULT_CHANNEL_ID, "Channel 1");
    ch.rundown = vec!["a".into(), "b".into(), "c".into()];
    ch.transition = Transition::new(TransitionKind::Mix, 10);
    c.channels.push(ch);
    c
}

#[test]
fn cue_take_next_clear() {
    let hub = GraphicsHub::new(config());
    let ch = DEFAULT_CHANNEL_ID;
    assert_eq!(hub.take(ch, None), Err(PlayoutError::NothingCued));
    let s = hub.next(ch).unwrap();
    assert_eq!(s.preview.as_deref(), Some("a"));
    let s = hub.take(ch, None).unwrap();
    assert_eq!(s.program.as_deref(), Some("a"));
    assert_eq!(s.preview.as_deref(), Some("b"), "auto-advance to next");
    assert_eq!(s.seq, 1);
    assert_eq!(s.transition.kind, TransitionKind::Mix);
    assert_eq!(s.transition.frames, 10);
    let s = hub
        .take(ch, Some(Transition::new(TransitionKind::Cut, 1)))
        .unwrap();
    assert_eq!(s.program.as_deref(), Some("b"));
    assert_eq!(s.transition.from.as_deref(), Some("a"));
    assert_eq!(s.transition.kind, TransitionKind::Cut);
    assert_eq!(s.preview.as_deref(), Some("c"));
    let s = hub.take(ch, None).unwrap();
    assert_eq!(s.preview, None, "end of rundown leaves preview empty");
    assert_eq!(hub.next(ch), Err(PlayoutError::EndOfRundown));
    let s = hub.cue(ch, "a").unwrap();
    assert_eq!(s.preview.as_deref(), Some("a"));
    assert!(matches!(
        hub.cue(ch, "zz"),
        Err(PlayoutError::UnknownItem(_))
    ));
    let s = hub.clear(ch, None).unwrap();
    assert_eq!(s.program, None);
    assert_eq!(s.transition.from.as_deref(), Some("c"));
    assert_eq!(s.transition.to, None);
    let seq = s.seq;
    // Clearing an empty program is a no-op (no new transition).
    assert_eq!(hub.clear(ch, None).unwrap().seq, seq);
    assert!(matches!(
        hub.take("nope", None),
        Err(PlayoutError::UnknownChannel(_))
    ));
    // Published snapshot is visible to the non-blocking reader.
    assert_eq!(
        hub.try_channel(ch).unwrap().unwrap().program,
        None,
        "reader sees the cleared state"
    );
}

#[test]
fn deleting_an_on_air_item_clears_program() {
    let hub = GraphicsHub::new(config());
    let ch = DEFAULT_CHANNEL_ID;
    hub.cue(ch, "b").unwrap();
    let before = hub.take(ch, None).unwrap();
    hub.cue(ch, "b").unwrap();
    let mut next = config();
    next.items.retain(|i| i.id != "b");
    next.channels[0].rundown.retain(|i| i != "b");
    hub.set_config(next);
    let s = hub.channel(ch).unwrap();
    assert_eq!(s.program, None, "deleted item is cut off air");
    assert_eq!(s.preview, None, "deleted item is uncued");
    assert!(s.seq > before.seq);
    assert_eq!(s.transition.kind, TransitionKind::Cut);
}

#[test]
fn default_channel_always_exists() {
    let hub = GraphicsHub::new(GraphicsConfig::default());
    assert!(hub.channel(DEFAULT_CHANNEL_ID).is_some());
    assert_eq!(hub.config().channels.len(), 1);
}

#[test]
fn mix_ramp_is_deterministic_from_ticks() {
    // Fade in over 4 frames: 64, 127, 191, 255 then settled.
    let ops: Vec<u8> = (0..6)
        .map(|t| mix_at(false, true, TransitionKind::Mix, 4, t).opacity)
        .collect();
    assert_eq!(ops, vec![63, 127, 191, 255, 255, 255]);
    assert!((0..6).all(|t| mix_at(false, true, TransitionKind::Mix, 4, t).layer == Some(Layer::To)));
    // Fade out: from layer ramps down to 0, then transparent.
    let out: Vec<MixFrame> = (0..5)
        .map(|t| mix_at(true, false, TransitionKind::Mix, 4, t))
        .collect();
    assert_eq!(
        out.iter().map(|f| f.opacity).collect::<Vec<_>>()[..4],
        [191, 127, 63, 0]
    );
    assert_eq!(out[4].layer, None);
    // Item → item dips through clear.
    let x: Vec<MixFrame> = (0..4)
        .map(|t| mix_at(true, true, TransitionKind::Mix, 4, t))
        .collect();
    assert_eq!(x[0].layer, Some(Layer::From));
    assert_eq!(x[1].opacity, 0);
    assert_eq!(x[2].layer, Some(Layer::To));
    assert_eq!(x[3].opacity, 255);
    // Cut is immediate.
    assert_eq!(
        mix_at(true, true, TransitionKind::Cut, 25, 0),
        MixFrame {
            layer: Some(Layer::To),
            opacity: 255
        }
    );
}

#[test]
fn scale_alpha_lut() {
    let mut out = Vec::new();
    scale_alpha(Some(&[0, 128, 255]), 3, 128, &mut out);
    assert_eq!(out, vec![0, 64, 128]);
    scale_alpha(None, 2, 255, &mut out);
    assert_eq!(out, vec![255, 255]);
}
