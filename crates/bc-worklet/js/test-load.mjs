// Node test for BcPlayer.load(): only the final chunk of the whole file may carry `last`.
// Usage: node js/test-load.mjs
import { BcPlayer, planChunks } from './player.js';

const SR = 48000;
const frames = SR * 3; // 3 s: several 4*CHUNK decode blocks
const posted = [];
const fake = {
  loads: [0, 0],
  epoch: 0,
  ctx: {
    sampleRate: SR,
    decodeAudioData: async () => ({
      length: frames,
      numberOfChannels: 1,
      getChannelData: () => new Float32Array(frames).fill(0.25),
    }),
  },
  node: { port: { postMessage: (m) => posted.push(m) } },
  _wait: new Map(),
  _cmd() {},
  _decodeWhole: BcPlayer.prototype._decodeWhole,
};
// resolve the ready wait as soon as the last chunk is posted
fake.node.port.postMessage = (m) => {
  posted.push(m);
  if (m.type === 'pcm' && m.last) for (const r of fake._wait.values()) r();
};
const ok = await BcPlayer.prototype.load.call(fake, 0, new ArrayBuffer(8), { startS: 0 });
const pcm = posted.filter((m) => m.type === 'pcm');
const lasts = pcm.filter((m) => m.last);
let covered = 0;
for (const m of pcm) {
  if (m.start !== covered) throw new Error(`gap/overlap at ${covered}, chunk starts ${m.start}`);
  covered += m.data.length / 2;
}
if (!ok) throw new Error('load superseded');
if (lasts.length !== 1 || !pcm[pcm.length - 1].last) throw new Error(`expected exactly the final chunk flagged, got ${lasts.length}`);
if (covered !== frames) throw new Error(`covered ${covered} of ${frames}`);

// start offset: a block that is cut by startS still ends on the file end
const p = planChunks({ at: 0, startFrame: 70000, frames: 65536 - 70000 > 0 ? 0 : 0, totalFrames: 200000 });
if (p.length !== 0) throw new Error('empty block yields no chunks');
const q = planChunks({ at: 65536, startFrame: 70000, frames: 65536 * 2 - 70000, totalFrames: 65536 * 2 });
if (q.length === 0 || !q[q.length - 1].last || q.slice(0, -1).some((c) => c.last)) throw new Error('tail block flags only its last chunk');
const r = planChunks({ at: 0, startFrame: 0, frames: 65536, totalFrames: 200000 });
if (r.some((c) => c.last)) throw new Error('a middle block never carries last');
const d = planChunks({ at: 0, startFrame: 0, frames: 40000, blockLast: true });
if (d.filter((c) => c.last).length !== 1) throw new Error('decoder-marked final block');
console.log(`bc-worklet load OK (${pcm.length} chunks, one last)`);
