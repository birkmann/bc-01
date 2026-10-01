// Live-update stress: proxies the real /api/ws and injects bursts of library.changed / invalidate /
// resync events while navigating (Home, lists, detail pages, entity links). Catches disposal panics
// that only show when realtime updates land while components mount and unmount.
// usage: node events-stress.mjs [base] [seconds]
import { chromium } from 'playwright-core'
import { ROUTES } from './routes.mjs'
import { guard } from './guard.mjs'
const base = process.argv[2] ?? 'http://127.0.0.1:8420'
const seconds = Number(process.argv[3] ?? 90)
await guard(base)
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable',
  args: ['--no-sandbox', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'] })
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
const bad = []
page.on('pageerror', (e) => { if (!/Transition was skipped/.test(e.message)) bad.push('pageerror: ' + e.message.slice(0, 3000)) })
page.on('console', (m) => { if (m.type() === 'error' && !m.text().startsWith('Failed to load resource')) bad.push('console: ' + m.text().slice(0, 3000)) })

const clients = []
await page.routeWebSocket(/\/api\/ws/, (ws) => {
  const server = ws.connectToServer()
  ws.onMessage((m) => server.send(m))
  server.onMessage((m) => ws.send(m))
  clients.push(ws)
})
let n = 0
const ev = (topic, payload) => JSON.stringify({ id: { epoch: 0, seq: 0 }, topic, payload })
const burst = () => {
  const frames = [
    ev('library.changed', { scope: 'import', tracks_added: 1 }),
    ev('invalidate', { entity: 'release', ids: [] }),
    ev('invalidate', { entity: 'track', ids: [] }),
    ev('invalidate', { entity: 'artist', ids: [] }),
    ev('invalidate', { entity: 'label', ids: [] }),
    ev('invalidate', { entity: 'tag', ids: [] }),
    ev('invalidate', { entity: 'job', ids: [] }),
    ev('analysis.progress', { done: n % 100, total: 100 }),
  ]
  if (n % 25 === 0) frames.push(ev('stream.resync', {}))
  n++
  for (const c of clients) for (const f of frames) { try { c.send(f) } catch {} }
}
const timer = setInterval(burst, 120)

const panicked = async () => (await page.locator('#bc-panic').count()) > 0 || bad.length > 0
const links = ['.alb .ent-link', '.hm-trk-title .ent-link', '.hm-hero-sub .ent-link', '.dt-row .ent-link', '.pp-release .ent-link', '.lib-labellink', '.pp-labellink', '.lib-artist', '.pl-title .ent-link', '.pl-artist .ent-link', '.pp-card-link', '.alb-link']
await page.goto(base + '/', { waitUntil: 'load' }); await page.waitForTimeout(1500)
// something in the player so the bar shows links
await page.goto(base + '/tracks', { waitUntil: 'load' }); await page.waitForTimeout(1500)
await page.waitForSelector('.dt-row').catch(() => {})
await page.locator('.dt-row').first().dblclick().catch(() => {})
const end = Date.now() + seconds * 1000
let step = 0
while (Date.now() < end && !(await panicked())) {
  step++
  const r = Math.random()
  if (r < 0.35) {
    const [, route] = ROUTES[Math.floor(Math.random() * ROUTES.length)]
    await page.evaluate((x) => { history.pushState({}, '', x); dispatchEvent(new PopStateEvent('popstate')) }, Math.random() < 0.4 ? '/' : route)
  } else if (r < 0.85) {
    const sel = links[Math.floor(Math.random() * links.length)]
    const els = page.locator(sel)
    const c = await els.count()
    if (c) await els.nth(Math.floor(Math.random() * Math.min(c, 8))).click({ timeout: 1500 }).catch(() => {})
  } else {
    await page.goBack().catch(() => {})
  }
  await page.waitForTimeout(150 + Math.random() * 600)
}
clearInterval(timer)
const where = new URL(page.url()).pathname
if (await page.locator('#bc-panic').count()) bad.unshift('PANIC overlay on ' + where + ': ' + (await page.locator('#bc-panic').innerText()).slice(0, 600))
console.log(bad.length ? bad.join('\n---\n') : `stress ok: ${step} steps, ${n} event bursts, no panics`)
await browser.close()
process.exit(bad.length ? 1 : 0)
