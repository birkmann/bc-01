// Node smoke test: instantiate the wasm directly (no AudioWorklet), push a tone, render, check the output.
// Usage: node js/test-node.mjs [path/to/bc_worklet.wasm]
import { readFile } from 'node:fs/promises';

const path = process.argv[2] || new URL('./bc_worklet.wasm', import.meta.url).pathname;
const { instance } = await WebAssembly.instantiate(await readFile(path), {});
const w = instance.exports;
const SR = 48000;
const h = w.bcw_new(SR, 1);

const frames = 12000;
const scratch = () => new Float32Array(w.memory.buffer, w.bcw_scratch(h), frames * 2);
let s = scratch();
for (let i = 0; i < frames; i++) s[i * 2] = s[i * 2 + 1] = 0.5 * Math.sin((2 * Math.PI * 440 * i) / SR);
if (!w.bcw_cue(h, 0, 1, 0, frames, 1, 0, 1, 0, 0, 0, 0)) throw new Error('cue rejected');
const got = w.bcw_push_pcm(h, 0, 1, 0, frames, 1);
if (got !== frames) throw new Error(`pushed ${got} of ${frames}`);
w.bcw_play(h, 0);

let peak = 0;
let rendered = 0;
for (let i = 0; i < 80; i++) {
  w.bcw_render(h, 128, 0);
  const out = new Float32Array(w.memory.buffer, w.bcw_out(h), 256);
  for (const x of out) peak = Math.max(peak, Math.abs(x));
  rendered += 128;
}
const snap = new Float64Array(w.memory.buffer, w.bcw_snapshot(h), 8 + 21);
console.log(`peak ${peak.toFixed(3)}, frames_played ${snap[0]}, deck A pos ${(snap[8] / SR).toFixed(3)} s, state ${snap[10]}`);
if (peak < 0.3) throw new Error('no audio rendered');
if (snap[10] !== 2) throw new Error('deck A is not playing');
w.bcw_free(h);
console.log('bc-worklet wasm OK');

// ---- second part: two decks, a blend, and the CPU cost of the graph in wasm ----
{
  const h2 = w.bcw_new(SR, 1);
  const tone = (f, secs) => {
    const n = Math.round(SR * secs);
    const a = new Float32Array(n * 2);
    for (let i = 0; i < n; i++) a[i * 2] = a[i * 2 + 1] = 0.3 * Math.sin((2 * Math.PI * f * i) / SR);
    return a;
  };
  const decks = [
    { pcm: tone(440, 12), at: 0, epoch: 1 },
    { pcm: tone(660, 12), at: 0, epoch: 2 },
  ];
  const feed = (d) => {
    const dk = decks[d];
    const total = dk.pcm.length / 2;
    while (dk.at < total) {
      const free = w.bcw_ring_free_frames(h2, d);
      const n = Math.min(total - dk.at, free, w.bcw_scratch_frames());
      if (n < 1024 && total - dk.at > n) break;
      new Float32Array(w.memory.buffer, w.bcw_scratch(h2), n * 2).set(dk.pcm.subarray(dk.at * 2, (dk.at + n) * 2));
      const got = w.bcw_push_pcm(h2, d, dk.epoch, dk.at, n, dk.at + n >= total ? 1 : 0);
      dk.at += got;
      if (got < n) break;
    }
  };
  w.bcw_cue(h2, 0, 1, 0, SR * 12, 1, 0, 1, 0, 0, 0, 0);
  w.bcw_cue(h2, 1, 2, 0, SR * 12, 1, 0, 1, 0, 0, 0, 0);
  w.bcw_play(h2, 0);
  const rms = (a, b, f) => {
    // single-bin DFT amplitude over the captured left channel
    let re = 0, im = 0;
    for (let i = a; i < b; i++) { const ph = (2 * Math.PI * f * i) / SR; re += cap[i] * Math.cos(ph); im += cap[i] * Math.sin(ph); }
    return (2 * Math.hypot(re, im)) / (b - a);
  };
  const total = SR * 8;
  const cap = new Float32Array(total);
  let pos = 0;
  let started = false;
  const t0 = performance.now();
  while (pos < total) {
    feed(0); feed(1);
    if (pos >= SR * 2 && !started) { started = true; w.bcw_transition(h2, 0, 1, 0, 4.0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0.2, 0, 0); }
    for (let k = 0; k < 64 && pos < total; k++) {
      w.bcw_render(h2, 128, 0);
      const out = new Float32Array(w.memory.buffer, w.bcw_out(h2), 256);
      for (let i = 0; i < 128; i++) cap[pos + i] = out[i * 2];
      pos += 128;
    }
  }
  const ms = performance.now() - t0;
  const a1 = rms(SR * 1, SR * 1.5, 440), b1 = rms(SR * 1, SR * 1.5, 660);
  const a2 = rms(SR * 4, SR * 4.5, 440), b2 = rms(SR * 4, SR * 4.5, 660);
  const a3 = rms(SR * 7, SR * 7.5, 440), b3 = rms(SR * 7, SR * 7.5, 660);
  console.log(`blend: before A ${a1.toFixed(3)} B ${b1.toFixed(3)} | mid A ${a2.toFixed(3)} B ${b2.toFixed(3)} | after A ${a3.toFixed(3)} B ${b3.toFixed(3)}`);
  console.log(`rendered 8 s of two decks + blend in ${ms.toFixed(0)} ms (${(8000 / ms).toFixed(0)}x real time, ${(ms / 8000 * 100).toFixed(1)} % of one core)`);
  if (!(a1 > 0.25 && b1 < 0.01 && a2 > 0.1 && b2 > 0.1 && a3 < 0.01 && b3 > 0.25)) throw new Error('blend did not crossfade A to B');
  w.bcw_free(h2);
  console.log('bc-worklet blend OK');
}
