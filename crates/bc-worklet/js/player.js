// Main-thread API for "play on this device": the bc-dsp mixer running in an AudioWorklet.
//
//   import { BcPlayer } from './player.js'
//   const p = await BcPlayer.create({ wasmUrl: '/bc_worklet.wasm', processorUrl: '/processor.js' })
//   await p.load(0, '/api/stream/42', { startS: 30 })          // deck A (decode + prime)
//   p.play(0)
//   await p.load(1, '/api/stream/43', { startS: 20, rate: 1.016, grid: {originS: .., periodS: ..} })
//   p.transition({ out: 0, inc: 1, kind: 'blend', lengthS: 15, echo: {send: .65, holdS: 3, upS: .3, bpm: 128} })
//   p.onEvent = (name, data) => ...      // 'ready' 'started' 'ended' 'advanced' 'transition' 'fade-done' 'parked' ...
//   p.state()                           // latest snapshot (positions in seconds), extrapolated to now
//
// Decoding: `decodeAudioData` (always available; the context resamples to its rate). Where WebCodecs
// `AudioDecoder` is available, pass `decoder: myStreamingDecoder` (an async generator yielding
// {pcm: Float32Array interleaved stereo at ctx.sampleRate, startFrame, totalFrames?, last?}; give
// totalFrames or mark the final block `last: true`) to stream instead of
// decoding a whole file. PCM reaches the worklet through a SharedArrayBuffer ring when the page is
// cross-origin isolated (the server sends COOP/COEP), and through postMessage transfer otherwise.

const KIND = { blend: 0, bass_swap: 1, filter: 2, echo_out: 3, cut: 4, eq_blend: 5 };
const QUANT = { off: 0, beat: 1, bar: 2, phrase: 3 };
const EVENTS = {
  1: 'ready', 2: 'started', 3: 'ended', 4: 'advanced', 5: 'underrun', 6: 'transition', 7: 'retimed',
  8: 'fade-done', 9: 'parked', 10: 'paused', 11: 'resumed',
};
const CHUNK = 16384; // frames per posted block

/**
 * Split one decoded block into posted chunks. Only the chunk that ends the whole file carries
 * `last` (a block of a longer file never does): `totalFrames` is the file length when the decoder
 * knows it, otherwise the decoder marks its final block with `blockLast`.
 */
export function planChunks({ at, startFrame, frames, totalFrames = 0, blockLast }) {
  const base = Math.max(at, startFrame);
  const blockEnd = base + frames;
  const isLastBlock = blockLast !== undefined ? !!blockLast : totalFrames > 0 && blockEnd >= totalFrames;
  const out = [];
  for (let off = 0; off < frames; off += CHUNK) {
    const n = Math.min(CHUNK, frames - off);
    out.push({ off, frames: n, start: base + off, last: isLastBlock && off + n >= frames });
  }
  return out;
}

export class BcPlayer {
  static async create({ ctx, wasmUrl, processorUrl, quality = 1, wasmModule } = {}) {
    ctx = ctx || new AudioContext({ latencyHint: 'interactive' });
    const module = wasmModule || (await WebAssembly.compileStreaming(fetch(wasmUrl)));
    await ctx.audioWorklet.addModule(processorUrl);
    const node = new AudioWorkletNode(ctx, 'bc-mixer', {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [2],
      processorOptions: { wasmModule: module, quality },
    });
    node.connect(ctx.destination);
    const p = new BcPlayer(ctx, node);
    await new Promise((res) => {
      const prev = node.port.onmessage;
      node.port.onmessage = (e) => {
        if (e.data.type === 'ready') {
          node.port.onmessage = (ev) => p._onMessage(ev.data);
          res();
        } else if (prev) prev(e);
      };
    });
    return p;
  }

  constructor(ctx, node) {
    this.ctx = ctx;
    this.node = node;
    this.onEvent = null;
    this.epoch = 1;
    this.snap = null;
    this.snapAt = 0;
    this.sab = typeof SharedArrayBuffer !== 'undefined' && self.crossOriginIsolated === true;
    this.loads = [0, 0, 0]; // load generation per deck, so a superseded load stops feeding
    this._wait = new Map();
  }

  get sampleRate() {
    return this.ctx.sampleRate;
  }
  get transport() {
    return this.sab ? 'sharedarraybuffer' : 'postMessage';
  }

  _onMessage(m) {
    if (m.type !== 'events') return;
    this.snap = m.snapshot;
    this.snapAt = this.ctx.currentTime;
    for (const e of m.events) {
      const name = EVENTS[e[0]] || 'unknown';
      if (name === 'ready') {
        const key = `${e[1]}:${e[2]}`;
        const w = this._wait.get(key);
        if (w) {
          this._wait.delete(key);
          w();
        }
      }
      if (this.onEvent) this.onEvent(name, e.slice(1));
    }
  }

  _cmd(fn, ...args) {
    this.node.port.postMessage({ type: 'cmd', fn, args });
  }

  /** Decode `source` (URL, ArrayBuffer or Blob) and prime `deck`. Resolves once the deck is ready to start. */
  async load(deck, source, { startS = 0, rate = 1, keylock = false, trim = 1, grid = null, decoder = null } = {}) {
    const gen = ++this.loads[deck];
    const epoch = ++this.epoch;
    this.node.port.postMessage({ type: 'drop', deck });
    const sr = this.ctx.sampleRate;
    const startFrame = Math.round(startS * sr);
    let pcmSource;
    if (decoder) {
      pcmSource = decoder(source, { sampleRate: sr, startFrame });
    } else {
      pcmSource = this._decodeWhole(source, sr);
    }
    let first = true;
    let lenFrames = 0;
    const readyP = new Promise((res) => this._wait.set(`${deck}:${epoch}`, res));
    for await (const block of pcmSource) {
      const { pcm, startFrame: at, totalFrames } = block;
      if (gen !== this.loads[deck]) return false; // superseded
      if (totalFrames) lenFrames = totalFrames;
      if (first) {
        first = false;
        const g = grid || {};
        this._cmd('bcw_cue', deck, epoch, startFrame, lenFrames, rate, keylock ? 1 : 0, trim, g.originS || 0, g.periodS || 0, g.confidence ?? 1, g.downbeat || 0);
      }
      const from = Math.max(0, startFrame - at);
      const data = pcm.subarray(from * 2);
      const plan = planChunks({ at, startFrame, frames: data.length / 2, totalFrames: lenFrames, blockLast: block.last });
      for (const c of plan) {
        if (gen !== this.loads[deck]) return false;
        const chunk = data.slice(c.off * 2, (c.off + c.frames) * 2);
        this.node.port.postMessage({ type: 'pcm', deck, epoch, start: c.start, last: c.last, data: chunk }, [chunk.buffer]);
        // gentle backpressure: the processor drains its queue at the rate it renders
        if (c.off > 0 && c.off % (CHUNK * 8) === 0) await new Promise((r) => setTimeout(r, 0));
      }
    }
    await readyP;
    return true;
  }

  async *_decodeWhole(source, sr) {
    let buf = source;
    if (typeof source === 'string') {
      const res = await fetch(source);
      if (!res.ok) throw new Error(`fetch ${source}: ${res.status}`);
      buf = await res.arrayBuffer();
    } else if (source instanceof Blob) {
      buf = await source.arrayBuffer();
    }
    const audio = await this.ctx.decodeAudioData(buf);
    const n = audio.length;
    const l = audio.getChannelData(0);
    const r = audio.numberOfChannels > 1 ? audio.getChannelData(1) : l;
    for (let off = 0; off < n; off += CHUNK * 4) {
      const m = Math.min(CHUNK * 4, n - off);
      const pcm = new Float32Array(m * 2);
      for (let i = 0; i < m; i++) {
        pcm[i * 2] = l[off + i];
        pcm[i * 2 + 1] = r[off + i];
      }
      yield { pcm, startFrame: off, totalFrames: n };
    }
  }

  play(deck) { this._cmd('bcw_play', deck); this.ctx.resume(); }
  pause() { this._cmd('bcw_pause'); }
  resume() { this._cmd('bcw_resume'); this.ctx.resume(); }
  stop() { this._cmd('bcw_stop'); }
  setVolume(v) { this._cmd('bcw_volume', v); }
  /** Key-lock vocoder size for decks loaded from now on: 'fast' | 'balanced' | 'high'. */
  setKeyLockQuality(q) { this._cmd('bcw_set_quality', q === 'fast' ? 0 : q === 'high' ? 2 : 1); }
  setRate(deck, rate, { keylock = false, glideS = 0 } = {}) { this._cmd('bcw_set_rate', deck, rate, keylock ? 1 : 0, glideS); }
  armGapless(nextDeck) { this._cmd('bcw_gapless', nextDeck == null ? -1 : nextDeck); }
  setStrip({ low = 1, mid = 1, high = 1, filter = 0, echoSend = 0 } = {}) { this._cmd('bcw_strip', low, mid, high, filter, echoSend); }
  retime(remainingS) { this._cmd('bcw_retime', remainingS); }
  cutNow() { this._cmd('bcw_cut_now'); }
  setEcho(on) { this._cmd('bcw_set_echo', on ? 1 : 0); }
  setSync(on) { this._cmd('bcw_set_sync', on ? 1 : 0); }
  nudge(deltaS) { this._cmd('bcw_nudge', deltaS); }

  /** Seek a loaded deck: the caller re-decodes from `seconds` via load() for streams; for decoded decks use seek(). */
  seek(deck, seconds, epoch) {
    this._cmd('bcw_seek', deck, epoch ?? ++this.epoch, Math.round(seconds * this.ctx.sampleRate));
  }

  /** Start a blend of `inc` into `out`. */
  transition({ out, inc, kind = 'blend', lengthS = 8, equalPower = true, echo = null, sync = null, parkTailS = 0.2, quantise = 'off', phaseLock = true }) {
    this._cmd(
      'bcw_transition', out, inc, KIND[kind] ?? 0, lengthS, equalPower ? 1 : 0,
      echo ? echo.send : 0, echo ? echo.holdS : 0, echo ? echo.upS : 0, echo?.bpm || 0,
      sync ? sync.rate : 0, sync?.keylock ? 1 : 0, sync?.fromBpm || 0, sync?.toBpm || 0,
      parkTailS, QUANT[quantise] ?? 0, phaseLock ? 1 : 0,
    );
  }

  /** Latest snapshot with positions in seconds, extrapolated to the context's current time. */
  state() {
    const s = this.snap;
    if (!s) return null;
    const sr = s[1];
    const dt = Math.max(0, this.ctx.currentTime - this.snapAt);
    const decks = [0, 1, 2].map((i) => {
      const b = 8 + i * 7;
      const playing = s[b + 2] === 2 && !s[3];
      return {
        positionS: s[b] / sr + (playing ? dt * s[b + 1] : 0),
        rate: s[b + 1],
        state: ['empty', 'cued', 'playing', 'ended'][s[b + 2]] || 'empty',
        ready: !!s[b + 3],
        bufferedS: s[b + 5],
        lengthS: s[b + 6] / sr,
      };
    });
    return { framesPlayed: s[0], active: s[2], paused: !!s[3], transitioning: !!s[4], xruns: s[5], peak: [s[6], s[7]], decks };
  }

  close() {
    this.node.disconnect();
    return this.ctx.close();
  }
}
