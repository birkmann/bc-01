// AudioWorkletProcessor hosting the bc-dsp mixer (wasm). Runs in AudioWorkletGlobalScope.
//
// Main thread -> processor messages (port):
//   {type:'cmd', fn:'bcw_play', args:[0]}                       call any whitelisted bcw_* command
//   {type:'pcm', deck, epoch, start, last, data:Float32Array}   interleaved stereo PCM at the context rate
//   {type:'ring', deck, sab, frames}                            attach a SharedArrayBuffer PCM ring
//   {type:'ring-start', deck, epoch, start} / {type:'ring-end', deck, total}
// Processor -> main: {type:'events', events:[[code,a..e]...], snapshot:Float64Array} every ~30 ms.
//
// SAB ring layout: Int32Array header [writeIdx, readIdx] (frames, monotonically increasing) at byte 0,
// then `frames * 2` interleaved f32 at byte 16. The main thread writes, the processor reads.

const CMD_WHITELIST = new Set([
  'bcw_cue', 'bcw_play', 'bcw_pause', 'bcw_resume', 'bcw_stop', 'bcw_seek', 'bcw_set_rate', 'bcw_volume', 'bcw_set_quality',
  'bcw_gapless', 'bcw_strip', 'bcw_transition', 'bcw_retime', 'bcw_cut_now', 'bcw_set_echo', 'bcw_set_sync',
  'bcw_nudge',
]);
const SNAP_LEN = 8 + 3 * 7;

class BcMixerProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { wasmModule, quality = 1 } = options.processorOptions;
    this.ready = false;
    this.pending = [[], [], []]; // per deck: queued {epoch,start,last,data,off}
    this.rings = [null, null, null];
    this.ev = new Float64Array(6);
    this.sinceReport = 0;
    this.eventsOut = [];
    WebAssembly.instantiate(wasmModule, {}).then((inst) => {
      this.wasm = inst.exports;
      this.host = this.wasm.bcw_new(sampleRate, quality);
      this.evPtr = this.wasm.bcw_alloc(6 * 8);
      this.ready = true;
      this.port.postMessage({ type: 'ready' });
    });
    this.port.onmessage = (e) => this.onMessage(e.data);
  }

  onMessage(m) {
    if (!this.ready) {
      (this.early ||= []).push(m);
      return;
    }
    switch (m.type) {
      case 'cmd':
        if (CMD_WHITELIST.has(m.fn)) this.wasm[m.fn](this.host, ...m.args);
        break;
      case 'pcm':
        this.pending[m.deck].push({ epoch: m.epoch, start: m.start, last: !!m.last, data: m.data, off: 0 });
        break;
      case 'drop': // discard queued PCM of a deck (a seek / new load)
        this.pending[m.deck].length = 0;
        if (this.rings[m.deck]) this.rings[m.deck].live = false;
        break;
      case 'ring':
        this.rings[m.deck] = {
          hdr: new Int32Array(m.sab, 0, 4),
          data: new Float32Array(m.sab, 16),
          frames: m.frames,
          epoch: 0,
          start: 0,
          read0: 0,
          live: false,
          total: -1,
        };
        break;
      case 'ring-start': {
        const r = this.rings[m.deck];
        if (r) {
          r.epoch = m.epoch;
          r.start = m.start;
          r.read0 = Atomics.load(r.hdr, 1);
          r.sent = 0;
          r.total = -1;
          r.live = true;
        }
        break;
      }
      case 'ring-end': {
        const r = this.rings[m.deck];
        if (r) r.total = m.total;
        break;
      }
    }
  }

  // Move queued PCM into the wasm rings, as much as fits.
  feed() {
    const w = this.wasm;
    const maxFrames = w.bcw_scratch_frames();
    for (let deck = 0; deck < 3; deck++) {
      const q = this.pending[deck];
      while (q.length) {
        const free = w.bcw_ring_free_frames(this.host, deck);
        if (free < 1024) break;
        const item = q[0];
        const total = item.data.length / 2;
        const n = Math.min(total - item.off, maxFrames, free);
        const scratch = new Float32Array(w.memory.buffer, w.bcw_scratch(this.host), n * 2);
        scratch.set(item.data.subarray(item.off * 2, (item.off + n) * 2));
        const endsItem = item.off + n >= total;
        const accepted = w.bcw_push_pcm(this.host, deck, item.epoch, item.start + item.off, n, endsItem && item.last ? 1 : 0);
        item.off += accepted;
        if (item.off >= total) q.shift();
        if (accepted < n) break;
      }
      const r = this.rings[deck];
      if (r && r.live) {
        const wi = Atomics.load(r.hdr, 0);
        let ri = Atomics.load(r.hdr, 1);
        while (wi - ri > 0) {
          const free = w.bcw_ring_free_frames(this.host, deck);
          if (free < 1024) break;
          const n = Math.min(wi - ri, maxFrames, free);
          const scratch = new Float32Array(w.memory.buffer, w.bcw_scratch(this.host), n * 2);
          for (let i = 0; i < n; i++) {
            const p = ((ri + i) % r.frames) * 2;
            scratch[i * 2] = r.data[p];
            scratch[i * 2 + 1] = r.data[p + 1];
          }
          const last = r.total >= 0 && ri + n - r.read0 >= r.total ? 1 : 0;
          const got = w.bcw_push_pcm(this.host, deck, r.epoch, r.start + (ri - r.read0), n, last);
          ri += got;
          Atomics.store(r.hdr, 1, ri);
          if (got < n) break;
        }
      }
    }
  }

  process(_inputs, outputs) {
    if (!this.ready) return true;
    const w = this.wasm;
    const out = outputs[0];
    const frames = out[0].length;
    this.feed();
    w.bcw_render(this.host, frames, 0);
    const buf = new Float32Array(w.memory.buffer, w.bcw_out(this.host), frames * 2);
    const l = out[0];
    const r = out[1] || out[0];
    for (let i = 0; i < frames; i++) {
      l[i] = buf[i * 2];
      r[i] = buf[i * 2 + 1];
    }
    this.sinceReport += frames;
    if (this.sinceReport >= sampleRate * 0.03) {
      this.sinceReport = 0;
      const events = [];
      while (w.bcw_poll_event(this.host, this.evPtr)) {
        events.push(Array.from(new Float64Array(w.memory.buffer, this.evPtr, 6)));
      }
      const snap = new Float64Array(w.memory.buffer, w.bcw_snapshot(this.host), SNAP_LEN).slice();
      this.port.postMessage({ type: 'events', events, snapshot: snap, ctxTime: currentTime });
    }
    return true;
  }
}

registerProcessor('bc-mixer', BcMixerProcessor);
