// Smoke: play a track via the UI, check the overview waveform painted, open the Deck view.
import { chromium } from 'playwright-core'
import os from 'node:os'
import path from 'node:path'
const base = process.argv[2] ?? 'http://127.0.0.1:8420'
import { guard } from './guard.mjs'
await guard(base)
const out = process.argv[3] ?? path.join(os.tmpdir(), 'bc-e2e', 'smoke')
import fs from 'node:fs'
fs.mkdirSync(out, { recursive: true })
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable',
  args: ['--no-sandbox', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'] })
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
const errs = []
page.on('pageerror', (e) => errs.push(e.message))
page.on('console', (m) => { if (m.type() === 'error') errs.push(m.text()) })
await page.goto(base + '/tracks', { waitUntil: 'networkidle' })
await page.waitForSelector('.dt-row')
await page.locator('.dt-row').first().dblclick()
await page.waitForTimeout(2500)
await page.screenshot({ path: out + '/playing.png' })
const info = await page.evaluate(async () => {
  const s = await (await fetch('/api/player/state')).json()
  const c = document.querySelector('.pl-wave canvas')
  return { status: s.status, current: s.current?.title, canvas: c ? [c.width, c.height] : null, crossOriginIsolated: self.crossOriginIsolated }
})
console.log(JSON.stringify(info))
await page.keyboard.press('v')
await page.waitForTimeout(2500)
await page.screenshot({ path: out + '/deck.png' })
console.log('errors:', errs.length ? errs.slice(0, 5) : 'none')
await browser.close()
