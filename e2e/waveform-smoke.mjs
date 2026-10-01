// WebGL2 + Canvas2D renderer smoke test (crates/bc-waveform/demo built with trunk).
// Draws all four styles (Bars, RGB spectral, three-band, mono) as overview strips plus zoomed
// deck strips, on WebGL2 and on forced Canvas2D, and checks colours and crispness per strip.
// usage: node waveform-smoke.mjs <dist dir> [out png prefix]
// env: CHROME (browser binary), WF_BCW2 (a .bcw2 wire file to render instead of the synth),
//      WF_DPR (device pixel ratio, default 1), WF_W (css width, default 1000)
import { chromium } from 'playwright-core'
import http from 'node:http'
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'

const dist = process.argv[2] ?? path.join(import.meta.dirname, '../crates/bc-waveform/demo/dist')
const outPrefix = process.argv[3] ?? path.join(os.tmpdir(), 'bc-e2e', 'waveform-smoke')
fs.mkdirSync(path.dirname(outPrefix), { recursive: true })
const dpr = Number(process.env.WF_DPR ?? 1)
const cssW = Number(process.env.WF_W ?? 1000)
const bytes = process.env.WF_BCW2 ? fs.readFileSync(process.env.WF_BCW2) : null
const types = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm' }
const server = http.createServer((req, res) => {
  const p = path.join(dist, req.url.split('?')[0] === '/' ? 'index.html' : req.url.split('?')[0])
  fs.readFile(p, (e, b) => {
    if (e) { res.writeHead(404); res.end(); return }
    res.writeHead(200, { 'content-type': types[path.extname(p)] ?? 'application/octet-stream' }); res.end(b)
  })
}).listen(0)
const base = `http://127.0.0.1:${server.address().port}`
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable',
  args: ['--no-sandbox', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'] })
let failed = false
for (const mode of ['webgl2', 'canvas2d']) {
  const ctx = await browser.newContext({ viewport: { width: cssW, height: 620 }, deviceScaleFactor: dpr })
  const page = await ctx.newPage()
  if (bytes) await page.addInitScript((b64) => { window.__wf_bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)) }, bytes.toString('base64'))
  const errs = []
  page.on('pageerror', (e) => errs.push(e.message))
  page.on('console', (m) => { if (['error', 'warning'].includes(m.type()) && !/integrity|404|willReadFrequently/.test(m.text())) errs.push(m.text()) })
  await page.goto(`${base}/?w=${cssW}&dpr=${dpr}${mode === 'canvas2d' ? '&fallback=1' : ''}`)
  await page.waitForFunction(() => window.__wf_done || window.__wf_error, null, { timeout: 60000 })
  // WebGL clears its drawing buffer after compositing, so analyse what the browser actually
  // presented: a screenshot decoded back into a 2d canvas
  const shot = (await page.screenshot()).toString('base64')
  const info = await page.evaluate(async ({ b64, dpr }) => {
    const img = new Image(); img.src = 'data:image/png;base64,' + b64; await img.decode()
    const t = document.createElement('canvas'); t.width = img.width; t.height = img.height
    const x = t.getContext('2d', { willReadFrequently: true }); x.drawImage(img, 0, 0)
    const strips = JSON.parse(window.__wf_strips)
    const bgc = [15, 17, 23]
    const isBg = (d, i) => Math.abs(d[i] - bgc[0]) + Math.abs(d[i + 1] - bgc[1]) + Math.abs(d[i + 2] - bgc[2]) < 24
    const stat = (r, fx0, fx1) => {
      const sx = Math.round((r.x + r.w * fx0) * dpr), sw = Math.round(r.w * (fx1 - fx0) * dpr)
      const sy = Math.round(r.y * dpr), sh = Math.round(r.h * dpr)
      const d = x.getImageData(sx, sy, sw, sh).data
      let lit = 0, R = 0, G = 0, B = 0, bgCols = 0
      for (let cx = 0; cx < sw; cx++) {
        let any = false
        for (let cy = 0; cy < sh; cy++) { if (!isBg(d, (cy * sw + cx) * 4)) any = true }
        if (!any) bgCols++
      }
      for (let i = 0; i < d.length; i += 4) if (!isBg(d, i)) { lit++; R += d[i]; G += d[i + 1]; B += d[i + 2] }
      return { lit: lit / (d.length / 4), rgb: lit ? [R / lit, G / lit, B / lit].map(Math.round) : null, bgCols: bgCols / sw }
    }
    const out = {}
    for (const s of strips) {
      out[s.name] = s.name.startsWith('bars') ? { played: stat(s, 0.28, 0.36), unplayed: stat(s, 0.42, 0.52), all: stat(s, 0, 1) } : stat(s, 0, 1)
    }
    return { backend: window.__wf_backend, error: window.__wf_error, strips: out }
  }, { b64: shot, dpr })
  await page.screenshot({ path: `${outPrefix}-${mode}.png` })
  const probs = []
  if (info.error) probs.push('error ' + info.error)
  if (info.backend !== mode) probs.push(`backend ${info.backend}`)
  if (errs.length) probs.push('console: ' + errs.slice(0, 3).join(' | '))
  if (!info.error) {
    for (const [name, s] of Object.entries(info.strips)) {
      const all = s.all ?? s
      if (!(all.lit > 0.05)) probs.push(`${name} nearly empty (lit ${all.lit})`)
      if (name.startsWith('bars')) {
        const p = s.played.rgb, u = s.unplayed.rgb
        if (!p || !(p[0] > 170 && p[1] > 170 && p[2] > 170)) probs.push(`${name} played part not near-white ${p}`)
        if (!u || !(u[2] > 150 && u[2] > u[0] + 60 && u[1] > u[0] + 40)) probs.push(`${name} unplayed part not cyan ${u}`)
        if (!(s.all.bgCols > 0.15 && s.all.bgCols < 0.6)) probs.push(`${name} no crisp gaps (empty columns ${s.all.bgCols})`)
      }
    }
  }
  console.log(mode, JSON.stringify(info.strips), probs.length ? 'PROBLEMS: ' + probs.join('; ') : 'ok')
  if (probs.length) failed = true
  await ctx.close()
}
await browser.close(); server.close()
process.exit(failed ? 1 : 0)
