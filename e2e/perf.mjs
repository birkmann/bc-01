// Perf probes against PLAN §12 targets, on the real 190k-track library.
// Usage: node perf.mjs [baseUrl] [serverPid]
import { chromium } from 'playwright-core'
import fs from 'node:fs'
import zlib from 'node:zlib'

const base = process.argv[2] ?? 'http://127.0.0.1:8420'
import { guard } from './guard.mjs'
await guard(base)
const serverPid = process.argv[3]
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable',
  args: ['--no-sandbox', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist', '--enable-precise-memory-info'] })
const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } })
const page = await ctx.newPage()
const res = {}

// -- cold start: navigation start -> first paint / first rows ---------------------------
const t0 = Date.now()
await page.goto(base + '/tracks', { waitUntil: 'commit' })
await page.waitForSelector('.dt-row .dt-cell:not(:has(.skeleton))', { timeout: 30000 })
res.cold_first_rows_ms = Date.now() - t0
res.paint = await page.evaluate(() => Object.fromEntries(performance.getEntriesByType('paint').map((p) => [p.name, Math.round(p.startTime)])))

// -- wasm download size ---------------------------------------------------------------
res.wasm = await page.evaluate(async () => {
  const r = performance.getEntriesByType('resource').filter((e) => e.name.endsWith('.wasm'))[0]
  return r ? { transfer: r.transferSize, decoded: r.decodedBodySize } : null
})

// -- scroll fps: continuous scroll for 4 s ------------------------------------------------
res.scroll = await page.evaluate(async () => {
  const el = document.querySelector('.dt-scroll')
  const frames = []
  let last = performance.now()
  const end = last + 4000
  await new Promise((resolve) => {
    const step = (t) => {
      frames.push(t - last)
      last = t
      el.scrollTop += 90
      if (t < end) requestAnimationFrame(step)
      else resolve()
    }
    requestAnimationFrame(step)
  })
  frames.shift()
  frames.sort((a, b) => a - b)
  const avg = frames.reduce((a, b) => a + b, 0) / frames.length
  return { frames: frames.length, avg_ms: +avg.toFixed(2), fps: +(1000 / avg).toFixed(1), p95_ms: +frames[Math.floor(frames.length * 0.95)].toFixed(1), worst_ms: +frames[frames.length - 1].toFixed(1) }
})

// -- jump to any of 190k rows: time until real rows (not skeletons) show ------------------
res.jump_ms = []
for (const frac of [0.39, 0.77, 0.97, 0.2]) {
  const ms = await page.evaluate(async (frac) => {
    const el = document.querySelector('.dt-scroll')
    const t = performance.now()
    el.scrollTop = (el.scrollHeight - el.clientHeight) * frac
    await new Promise((resolve) => {
      const tick = () => {
        const rows = [...document.querySelectorAll('.dt-row')]
        if (rows.length && rows.every((r) => !r.querySelector('.skeleton'))) resolve()
        else requestAnimationFrame(tick)
      }
      setTimeout(tick, 30)
    })
    return Math.round(performance.now() - t)
  }, frac)
  res.jump_ms.push(ms)
}

// -- search latency: keystroke -> result list changed ----------------------------------------
await page.evaluate(() => (document.querySelector('.dt-scroll').scrollTop = 0))
res.search_ms = []
const input = page.locator('.topbar input[type=search]')
for (const word of ['techno', 'dub', 'ambient', 'house']) {
  await input.fill('')
  await page.waitForTimeout(1200)
  const before = await page.locator('.page-header .sub').innerText()
  const t = Date.now()
  await input.type(word, { delay: 0 })
  await page.waitForFunction((b) => { const s = document.querySelector('.page-header .sub'); return s && s.textContent && s.textContent !== b }, before, { timeout: 15000 }).catch(() => {})
  res.search_ms.push(Date.now() - t)
}

// -- memory ----------------------------------------------------------------------------------------
res.heap_mb = await page.evaluate(() => Math.round(performance.memory.usedJSHeapSize / 1048576))
res.dom_nodes = await page.evaluate(() => document.getElementsByTagName('*').length)
// browser process RSS (renderer + GPU + browser; includes the wasm linear memory)
{
  let total = 0
  for (const d of fs.readdirSync('/proc').filter((x) => /^\d+$/.test(x))) {
    try {
      if (fs.readFileSync(`/proc/${d}/cmdline`, 'utf8').includes('playwright_chromiumdev_profile')) total += parseInt(/VmRSS:\s+(\d+)/.exec(fs.readFileSync(`/proc/${d}/status`, 'utf8'))[1])
    } catch {}
  }
  res.browser_rss_mb = Math.round(total / 1024)
}
if (serverPid) {
  const st = fs.readFileSync(`/proc/${serverPid}/status`, 'utf8')
  res.server_rss_mb = Math.round(parseInt(/VmRSS:\s+(\d+)/.exec(st)[1]) / 1024)
}
console.log(JSON.stringify(res, null, 2))
await browser.close()
