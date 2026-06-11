# Why Multiview exists

*A note from the author.*

I started Multiview as a side project while I was recovering from a health issue. I needed
something to point my mind at — a constructive outlet, something to build — and this is what I
reached for. That's the honest origin: it began small, as a way to keep my hands busy and my
head engaged while I got well.

What it grew into is a tool I wish I'd had earlier.

## The problem I kept running into

The live and broadcast technology world is full of genuinely fascinating engineering — and almost
all of it is hard to *practise*. Not hard to read about; hard to actually get your hands on. The
concepts that make live production work tend to live inside expensive equipment, behind high price
tags, in facilities you can only learn in if you already work somewhere that owns the gear.

A short, incomplete list of the things I mean:

- **Precise time synchronisation** — locking many devices to a shared reference so frames line up
  to the sample (PTP / genlock).
- **Video codecs** — what really happens when you encode and decode a stream, and why the choices
  matter.
- **Audio and data de-embedding and embedding** — pulling audio and ancillary data out of a video
  signal and putting it back, the way broadcast facilities do every day.
- **Multiview compositing on the GPU** — taking many live feeds and laying them out into one clean
  wall, in real time, without the picture falling apart.
- **IP transport** — moving video around with RTSP, HLS, SRT, RTMP, NDI and friends.
- **Audio over IP** — carrying broadcast-grade audio across a network using open standards.
- **Resilient continuous output** — the unglamorous, load-bearing skill of *never dropping a
  frame*, no matter how badly an input behaves.

That's the *space* Multiview explores, not a claim that it does all of it today: some of these you
can already exercise, and the most ambitious ones (native IP-broadcast transport and hardware
reference-clock timing among them) are still on the roadmap — the [feature status
matrix](FEATURES.md) is honest about which is which.

If you already work in a facility that does these things, you can learn them on the job. If you
don't, the door is mostly closed. The hardware that teaches these ideas is costly, and the tooling
around it is often locked up. That's a real barrier to entry for an industry that's hard to break
into precisely *because* the equipment is hard to get near.

## What I want it to be

I want Multiview to be a great, genuinely *functional* tool — principally for non-commercial use —
that lets emerging artists and technicians get hands-on with these concepts on hardware they
already own. A laptop, a spare mini-PC, a single GPU. Something you can run, break, read the source
of, change, and run again.

It is **software for learning and practising**, and I want to be clear about what that means. It
lets you *exercise* these ideas — composite a real multiview wall, watch what an output clock does
when an input dies, see how a stream survives a flaky source — on commodity hardware. It is **not**
a replacement for professional equipment, and it does not pretend to be a broadcast truck. The
point isn't to replace the expensive gear; it's to lower the barrier to *understanding* what that
gear does, so the next person finds the field a little easier to step into.

I'll also be honest about where it stands: this is **early-stage, in active development**. A lot is
designed and documented; a meaningful amount is built and tested; some of the most ambitious pieces
(native IP-broadcast I/O and reference-clock timing among them) are on the
[roadmap](ROADMAP.md), not yet in your hands. The
[feature status matrix](FEATURES.md) tells you the truth, feature by feature — no overclaiming.
If you want to see the engineering reasoning, it's all written down in
[docs/research](docs/research/) and [docs/decisions](docs/decisions/).

## Why it's licensed and built the way it is

Two choices flow directly from that mission, and they're opinions I hold rather than neutral
facts — so I'll own them as mine.

**It should stay free for the people it's meant to serve.** Multiview is source-available under the
**Multiview Source-Available Non-Commercial License**: free for genuine personal, home, hobby and
other non-commercial use (with three defined free exceptions), while commercial use is licensed
separately. I chose that deliberately. Keeping it free for learners and home users is the whole
point; the commercial licence is what lets the project sustain itself without putting a price tag
in front of the student, the hobbyist, or the artist who's just trying to understand how this works.
The full terms — and what counts as which — are in [the licensing section](README.md#licensing)
and the [LICENSE](LICENSE).

**It should favour open standards.** Where I can build on an openly-published standard, I do — so
that what you learn here transfers, and so that Multiview can interoperate without forcing you
through gatekept, commercial-only SDKs. That's a learning argument as much as a technical one: open
standards are the ones you can read, reason about, and carry with you to the next tool. You can see
this preference play out in how the audio-over-IP path is approached — the specifics, with the
evidence behind them, live in the README's
[**Audio over IP**](README.md#audio-over-ip) section.

## An invitation

If you're an emerging tech, an artist, a student, or just someone curious about how live video
actually gets made — this is for you. Clone it, run the
[quick start](README.md#quick-start), point a tile at something, and poke at it. Read the source.
Break it and tell me how. Open an issue or a pull request — the
[contributing guide](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md) will get you
started, and everyone is welcome here.

It started as a way to keep me going through a hard stretch. I'd be glad if it helped you get into
a field that's worth getting into.

— Troy Kelly
