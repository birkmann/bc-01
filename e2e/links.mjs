// Entity-link walk: click artist / album / label links in cards, rows and the player bar, go back,
// repeat, then walk every route. Catches disposal panics from links inside reactive views.
// usage: node links.mjs [base]
import { chromium } from 'playwright-core'
import { ROUTES } from './routes.mjs'
import { guard } from './guard.mjs'
const base = process.argv[2] ?? 'http://127.0.0.1:8420'
await guard(base)
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable',
  args: ['--no-sandbox', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'] })
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
const bad = []
page.on('pageerror', (e) => bad.push('pageerror: ' + e.message))
page.on('console', (m) => { if (m.type() === 'error' && !m.text().startsWith('Failed to load resource')) bad.push('console: ' + m.text().slice(0, 1500)) })
page.on('response', (r) => { if (r.status() >= 400 && !/\/api\/(art|waveform|stream)/.test(r.url())) console.log('http', r.status(), r.url()) })
const panicked = async (where) => {
  if (await page.locator('#bc-panic').count()) { bad.push(`PANIC after ${where}`); return true }
  return bad.length > 0
}
async function clickFirst(sel, where) {
  const el = page.locator(sel).first()
  if (!(await el.count())) { console.log('skip (none):', where); return false }
  await el.scrollIntoViewIfNeeded().catch(() => {})
  await el.click({ timeout: 3000 }).catch((e) => console.log('click failed', where, e.message.split('\n')[0]))
  await page.waitForTimeout(900)
  console.log('clicked', where, '->', new URL(page.url()).pathname)
  return true
}
const steps = [
  ['/', '.alb .ent-link[href^="/artists/"]', 'home album card artist'],
  ['/', '.hm-trk-title .ent-link', 'home top10 title'],
  ['/', '.hm-hero-sub .ent-link[href^="/labels/"]', 'home hero label'],
  ['/albums', '.alb .ent-link[href^="/artists/"]', 'albums grid artist'],
  ['/tracks', '.dt-row .ent-link[href^="/albums/"]', 'tracks title'],
  ['/tracks', '.dt-row .ent-link[href^="/artists/"]', 'tracks artist'],
  ['/labels', '.pp-card-link', 'label card'],
]
for (let lap = 0; lap < 2 && !bad.length; lap++) {
  for (const [route, sel, where] of steps) {
    await page.goto(base + route, { waitUntil: 'networkidle' }).catch(() => {})
    await page.waitForTimeout(800)
    await clickFirst(sel, where)
    if (await panicked(where)) break
    // on the page we landed on, follow one more link upward, then back
    await clickFirst('.pp-release .ent-link, .lib-labellink, .pp-labellink, .lib-artist', where + ' / next hop')
    if (await panicked(where + ' / next hop')) break
    await page.goBack().catch(() => {}); await page.waitForTimeout(600)
    if (await panicked(where + ' / back')) break
  }
}
// player bar: start a track, then follow its title + artist links
if (!bad.length) {
  await page.goto(base + '/tracks', { waitUntil: 'networkidle' })
  await page.waitForSelector('.dt-row')
  await page.locator('.dt-row').first().dblclick()
  await page.waitForTimeout(1500)
  await clickFirst('.pl-title .ent-link', 'player title')
  await panicked('player title')
  await clickFirst('.pl-artist .ent-link', 'player artist')
  await panicked('player artist')
  for (const [name, route] of ROUTES) {
    await page.evaluate((r) => { history.pushState({}, '', r); dispatchEvent(new PopStateEvent('popstate')) }, route)
    await page.waitForTimeout(400)
    if (await panicked('route ' + name)) break
  }
}
console.log(bad.length ? bad.join('\n') : 'links ok: no panics')
await browser.close()
process.exit(bad.length ? 1 : 0)
