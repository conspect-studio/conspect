// Live HLS reference stream with alternative renditions (#418): CMAF fMP4, one video, two AAC renditions and one
// segmented WebVTT subtitle rendition.
//   node gen-tracks-stream.mjs <outdir> <seconds> [--size 1280x720] [--offset <s>] [--mpegts wrap|full]
//                              [--gen <tag>] [--window 10]
// ffmpeg writes video.m3u8 / audio.m3u8 ("Program", en, 440 Hz) / audio1.m3u8 ("Commentary", de, 880 Hz) and
// master-ff.m3u8; this process writes subs.m3u8 + sub-<gen>-<seq>.vtt beside every video segment (same media
// sequence, EXTINF, PDT) and master.m3u8 (NAME/LANGUAGE/DEFAULT, SUBTITLES group).
// Each .vtt: X-TIMESTAMP-MAP=MPEGTS:<m>,LOCAL:00:00:00.000 where m = the video tfdt of that segment in 90 kHz,
// modulo 2^33 (--mpegts wrap, default) or the full value (--mpegts full, the wrong variant, for the timing probe);
// one cue per segment from LOCAL 0.000 to 1.500 with the text "CUE <seq> T=<tfdt seconds, 3 decimals> PDT=<the
// segment's EXT-X-PROGRAM-DATE-TIME in epoch ms>" (PDT lets a test time a cue without trusting X-TIMESTAMP-MAP).
// --offset: the media timeline starts there (ffmpeg -output_ts_offset + movflags=frag_discont keeps it in the
// tfdt), e.g. 100000 s (27.8 h, past the 33-bit wrap at 95443.7 s) like an engine that has run for a day.
// Exits (and kills ffmpeg) on SIGTERM/SIGINT or when ffmpeg ends.
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { parseArgs } from './lib.mjs';

const args = parseArgs(process.argv.slice(2), { size: '1280x720', offset: 0, mpegts: 'wrap', window: 10, vbr: '3M' });
const [out, secondsArg] = args._;
if (!out || !secondsArg) { console.error('usage: gen-tracks-stream.mjs <outdir> <seconds> [--offset s] [--mpegts wrap|full]'); process.exit(2); }
const gen = String(args.gen && args.gen !== 'true' ? args.gen : Math.floor(Date.now() / 1000));
const FFMPEG = process.env.FFMPEG || '/usr/bin/ffmpeg';
const ROLLOVER = 2 ** 33;
fs.mkdirSync(out, { recursive: true });
for (const f of fs.readdirSync(out)) if (/\.(m3u8|m4s|mp4|vtt|tmp)$/.test(f)) fs.rmSync(path.join(out, f), { force: true });

const ff = spawn(FFMPEG, ['-hide_banner', '-loglevel', 'error', '-nostdin',
  '-re', '-f', 'lavfi', '-i', `testsrc2=size=${args.size}:rate=30000/1001`,
  '-re', '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000',
  '-re', '-f', 'lavfi', '-i', 'sine=frequency=880:sample_rate=48000',
  '-t', String(secondsArg),
  '-c:v', 'libx264', '-preset', 'veryfast', '-tune', 'zerolatency', '-profile:v', 'high', '-pix_fmt', 'yuv420p',
  '-b:v', String(args.vbr), '-maxrate', String(args.vbr), '-bufsize', String(args.vbr), '-g', '30', '-keyint_min', '30', '-sc_threshold', '0', '-bf', '0',
  '-c:a', 'aac', '-b:a', '128k', '-ac', '2', '-ar', '48000',
  '-map', '0:v', '-map', '1:a', '-map', '2:a',
  ...(Number(args.offset) > 0 ? ['-output_ts_offset', String(args.offset)] : []),
  '-f', 'hls', '-hls_time', '2', '-hls_list_size', String(args.window), '-hls_segment_type', 'fmp4',
  '-hls_segment_options', 'movflags=+frag_discont',
  '-hls_flags', 'delete_segments+independent_segments+program_date_time',
  '-var_stream_map', 'v:0,agroup:aud,name:video a:0,agroup:aud,default:yes,language:en,name:audio a:1,agroup:aud,language:de,name:audio1',
  '-master_pl_name', 'master-ff.m3u8', '-hls_segment_filename', `seg-%v-${gen}-%05d.m4s`, '-hls_fmp4_init_filename', `init-%v-${gen}.mp4`, '%v.m3u8'
], { cwd: out, stdio: ['ignore', 'ignore', 'inherit'] });
let stopping = false;
const stop = (code) => { stopping = true; try { ff.kill('SIGKILL'); } catch { /* gone */ } process.exit(code); };
ff.once('exit', (code) => { if (!stopping) { console.error(`ffmpeg exited ${code}`); process.exit(code === 0 ? 0 : 1); } });
for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(sig, () => stop(0));
process.on('exit', () => { try { ff.kill('SIGKILL'); } catch { /* gone */ } });

/** Walk ISO BMFF boxes: returns the payload offset of the first box path match (e.g. ['moof','traf','tfdt']). */
function findBox(buf, names, start = 0, end = buf.length) {
  let off = start;
  while (off + 8 <= end) {
    let size = buf.readUInt32BE(off); const type = buf.toString('latin1', off + 4, off + 8); let header = 8;
    if (size === 1) { size = Number(buf.readBigUInt64BE(off + 8)); header = 16; } else if (size === 0) size = end - off;
    if (size < header) return null;
    if (type === names[0]) return names.length === 1 ? { off: off + header, end: off + size } : findBox(buf, names.slice(1), off + header, off + size);
    off += size;
  }
  return null;
}
function timescaleOf(file) {
  const b = fs.readFileSync(file); const m = findBox(b, ['moov', 'trak', 'mdia', 'mdhd']); if (!m) return null;
  return b[m.off] === 1 ? b.readUInt32BE(m.off + 4 + 16) : b.readUInt32BE(m.off + 4 + 8);
}
function tfdtOf(file) {
  const b = fs.readFileSync(file); const t = findBox(b, ['moof', 'traf', 'tfdt']); if (!t) return null;
  return b[t.off] === 1 ? Number(b.readBigUInt64BE(t.off + 4)) : b.readUInt32BE(t.off + 4);
}
const vttTime = (s) => { const ms = Math.round(s * 1000); const h = Math.floor(ms / 3600000); const m = Math.floor(ms / 60000) % 60; const sec = (ms / 1000) % 60; return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}:${sec.toFixed(3).padStart(6, '0')}`; };
const writeAtomic = (name, text) => { const tmp = path.join(out, name + '.tmp'); fs.writeFileSync(tmp, text); fs.renameSync(tmp, path.join(out, name)); };

let timescale = null; const vtt = new Map(); let masterDone = false; let lastPlaylist = '';
function tick() {
  let text; try { text = fs.readFileSync(path.join(out, 'video.m3u8'), 'utf8'); } catch { return; }
  if (text === lastPlaylist) return;
  const lines = text.split('\n'); const seqLine = lines.find((l) => l.startsWith('#EXT-X-MEDIA-SEQUENCE:'));
  if (!seqLine) return;
  let seq = Number(seqLine.split(':')[1]); const entries = []; let inf = null; let pdt = null;
  for (const l of lines) {
    if (l.startsWith('#EXTINF:')) inf = l; else if (l.startsWith('#EXT-X-PROGRAM-DATE-TIME:')) pdt = l;
    else if (l && !l.startsWith('#')) { entries.push({ seq: seq++, inf, pdt, uri: l.trim() }); inf = null; pdt = null; }
  }
  if (!entries.length) return;
  if (timescale === null) { const map = /#EXT-X-MAP:URI="([^"]+)"/.exec(text); if (!map) return; try { timescale = timescaleOf(path.join(out, map[1])); } catch { return; } if (!timescale) return; }
  for (const e of entries) {
    if (vtt.has(e.seq)) continue;
    let tfdt; try { tfdt = tfdtOf(path.join(out, e.uri)); } catch { return; }
    if (tfdt === null) return;
    const sec = tfdt / timescale; const full = Math.round(tfdt * 90000 / timescale);
    const mpegts = args.mpegts === 'full' ? full : full % ROLLOVER;
    const name = `sub-${gen}-${String(e.seq).padStart(5, '0')}.vtt`;
    const pdtMs = e.pdt ? Date.parse(e.pdt.slice('#EXT-X-PROGRAM-DATE-TIME:'.length).replace(/([+-]\d\d)(\d\d)$/, '$1:$2')) : NaN;
    const body = `WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:${mpegts},LOCAL:00:00:00.000\n\n${e.seq}\n${vttTime(0)} --> ${vttTime(1.5)}\nCUE ${e.seq} T=${sec.toFixed(3)} PDT=${pdtMs}\n`;
    writeAtomic(name, body); vtt.set(e.seq, { name, at: Date.now() });
  }
  const head = lines.filter((l) => /^#EXT(M3U|-X-VERSION|-X-TARGETDURATION|-X-MEDIA-SEQUENCE|-X-INDEPENDENT-SEGMENTS)/.test(l));
  const body = entries.map((e) => [e.inf, e.pdt, vtt.get(e.seq).name].filter(Boolean).join('\n')).join('\n');
  writeAtomic('subs.m3u8', `${head.join('\n')}\n${body}\n`);
  lastPlaylist = text;
  const first = entries[0].seq;
  for (const [s, v] of vtt) if (s < first - 2) { fs.rmSync(path.join(out, v.name), { force: true }); vtt.delete(s); }
  if (!masterDone) {
    let ffm; try { ffm = fs.readFileSync(path.join(out, 'master-ff.m3u8'), 'utf8'); } catch { return; }
    const inf = /#EXT-X-STREAM-INF:([^\n]*)/.exec(ffm); if (!inf) return;
    const bw = (/(?:^|,)BANDWIDTH=(\d+)/.exec(inf[1]) || [])[1]; const codecs = (/CODECS="([^"]+)"/.exec(inf[1]) || [])[1]; const res = (/RESOLUTION=(\d+x\d+)/.exec(inf[1]) || [])[1];
    writeAtomic('master.m3u8', ['#EXTM3U', '#EXT-X-VERSION:7', '#EXT-X-INDEPENDENT-SEGMENTS',
      '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="Program",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",URI="audio.m3u8"',
      '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="Commentary",LANGUAGE="de",DEFAULT=NO,AUTOSELECT=YES,CHANNELS="2",URI="audio1.m3u8"',
      '#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="Captions",LANGUAGE="en",DEFAULT=NO,AUTOSELECT=NO,URI="subs.m3u8"',
      `#EXT-X-STREAM-INF:BANDWIDTH=${bw},CODECS="${codecs}",RESOLUTION=${res},AUDIO="aud",SUBTITLES="subs"`, 'video.m3u8', ''].join('\n'));
    masterDone = true;
  }
}
setInterval(tick, 100);
