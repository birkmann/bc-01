// SPA navigation walk with playback and panels open: catches disposal/reactivity panics that only
// show when components unmount while background loops run.
import { chromium } from 'playwright-core'
import { ROUTES } from './routes.mjs'
const base = process.argv[2] ?? 'http://127.0.0.1:8420'
import { guard } from './guard.mjs'
await guard(base)
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable',
  args: ['--no-sandbox', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'] })
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
const bad = []
page.on('pageerror', (e) => bad.push('pageerror: ' + e.message))
await page.goto(base + '/tracks', { waitUntil: 'networkidle' })
await page.waitForSelector('.dt-row')
await page.locator('.dt-row').first().dblclick()
await page.waitForTimeout(1200)
await page.keyboard.press('q')
for (let lap = 0; lap < 2; lap++) {
  for (const [name, route] of ROUTES) {
    await page.evaluate((r) => { const a = [...document.querySelectorAll('a[href]')].find((x) => x.getAttribute('href') === r); if (a) a.click(); else { history.pushState({}, '', r); dispatchEvent(new PopStateEvent('popstate')) } }, route)
    await page.waitForTimeout(500)
    if (lap === 1 && name === 'tracks') { await page.keyboard.press('v'); await page.waitForTimeout(400); await page.keyboard.press('Escape') }
    if (await page.locator('#bc-panic').count()) { bad.push(`PANIC on ${name}: ` + (await page.locator('#bc-panic pre').innerText()).slice(0, 400)); break }
  }
  if (bad.length) break
}
// the sets detail + arrange (needs a real set id)
const sets = await page.evaluate(async () => (await fetch('/api/sets')).json())
const id = (Array.isArray(sets) ? sets : sets.items)?.[0]?.id
if (id && !bad.length) {
  for (const v of ['plan', 'arrange', 'plan', 'arrange']) {
    await page.evaluate(([i, v]) => { history.pushState({}, '', `/sets/${i}?view=${v}`); dispatchEvent(new PopStateEvent('popstate')) }, [id, v])
    await page.waitForTimeout(1800)
    if (await page.locator('#bc-panic').count()) { bad.push(`PANIC on set ${v}: ` + (await page.locator('#bc-panic pre').innerText()).slice(0, 400)); break }
  }
}
console.log(bad.length ? bad.join('\n') : 'walk ok: no panics across ' + ROUTES.length + ' routes x2 with playback and the planner open')
await browser.close()
process.exit(bad.length ? 1 : 0)
