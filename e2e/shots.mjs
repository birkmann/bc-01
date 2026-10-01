// Usage: node shots.mjs [baseUrl] [outDir] [routeFilter]
// Screenshots every route at 375/768/1440 and records console/page errors and failed requests.
import { chromium } from 'playwright-core'
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'
import { ROUTES, VIEWPORTS } from './routes.mjs'

const base = process.argv[2] ?? 'http://127.0.0.1:8420'
import { guard } from './guard.mjs'
await guard(base)
const out = process.argv[3] ?? path.join(os.tmpdir(), 'bc-e2e', 'shots')
const filter = process.argv[4]
fs.mkdirSync(out, { recursive: true })

const browser = await chromium.launch({
  executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable',
  args: ['--no-sandbox', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'],
})
const problems = []
for (const [w, h] of VIEWPORTS) {
  const ctx = await browser.newContext({ viewport: { width: w, height: h }, deviceScaleFactor: 1 })
  const page = await ctx.newPage()
  const tag = { cur: '' }
  page.on('console', (m) => { if (['error', 'warning'].includes(m.type())) problems.push(`[${w}] ${tag.cur} console.${m.type()}: ${m.text()}`) })
  page.on('pageerror', (e) => problems.push(`[${w}] ${tag.cur} pageerror: ${e.message}`))
  page.on('requestfailed', (r) => problems.push(`[${w}] ${tag.cur} requestfailed: ${r.url()} ${r.failure()?.errorText}`))
  for (const [name, route] of ROUTES) {
    if (filter && !name.includes(filter)) continue
    tag.cur = name
    await page.goto(base + route, { waitUntil: 'networkidle' }).catch(() => {})
    await page.waitForTimeout(900)
    if (await page.locator('#bc-panic').count()) problems.push(`[${w}] ${tag.cur} PANIC overlay: ` + (await page.locator('#bc-panic pre').innerText()).slice(0, 300))
    await fs.promises.mkdir(path.join(out, String(w)), { recursive: true })
    await page.screenshot({ path: path.join(out, String(w), `${name}.png`) })
    console.log('shot', w, name)
  }
  await ctx.close()
}
await browser.close()
fs.writeFileSync(path.join(out, 'problems.log'), problems.join('\n') + '\n')
console.log(problems.length ? `${problems.length} problems (see problems.log)` : 'no console problems')
