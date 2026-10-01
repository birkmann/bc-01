// Screenshot one route in every built-in theme (visual contrast check). Usage: node themes.mjs [base] [out] [route]
import { chromium } from 'playwright-core'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
const base = process.argv[2] ?? 'http://127.0.0.1:8420'
import { guard } from './guard.mjs'
await guard(base)
const out = process.argv[3] ?? path.join(os.tmpdir(), 'bc-e2e', 'themes')
const route = process.argv[4] ?? '/tracks'
fs.mkdirSync(out, { recursive: true })
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable', args: ['--no-sandbox'] })
for (const [id, mode] of [['dark', 'dark'], ['light', 'light'], ['oled', 'dark'], ['midnight', 'dark'], ['paper', 'light'], ['contrast', 'dark']]) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } })
  const store = { dark: id === 'dark' || mode === 'light' ? 'builtin:dark' : `builtin:${id}`, light: mode === 'light' ? `builtin:${id}` : 'builtin:light', mode, themes: [] }
  await ctx.addInitScript((s) => localStorage.setItem('bc:theme:v3', JSON.stringify(s)), store)
  const page = await ctx.newPage()
  await page.route('**/api/ui-state/themes', (r) => r.fulfill({ status: 404, body: '{}' }))
  await page.goto(base + route, { waitUntil: 'networkidle' })
  await page.waitForTimeout(800)
  await page.screenshot({ path: `${out}/${id}.png` })
  await ctx.close()
}
await browser.close()
console.log('done')
