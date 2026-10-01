// "This device" target: play through the AudioWorklet host and check the clock advances.
import { chromium } from 'playwright-core'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
const base = process.argv[2] ?? 'http://127.0.0.1:8420'
import { guard } from './guard.mjs'
await guard(base)
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable',
  args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'] })
const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } })
await ctx.addInitScript(() => localStorage.setItem('bc:player:target', 'browser'))
const page = await ctx.newPage()
const errs = []
page.on('pageerror', (e) => errs.push(e.message))
page.on('console', (m) => { if (m.type() === 'error') errs.push(m.text()) })
await page.goto(base + '/tracks', { waitUntil: 'networkidle' })
await page.waitForSelector('.dt-row')
await page.locator('.dt-row').first().dblclick()
await page.waitForTimeout(9000)
const t = await page.locator('.pl-seek .pt').first().innerText()
const title = await page.locator('.pl-title').innerText()
console.log(JSON.stringify({ title, elapsed: t, isolated: await page.evaluate(() => self.crossOriginIsolated) }))
fs.mkdirSync(path.join(os.tmpdir(), 'bc-e2e'), { recursive: true })
await page.screenshot({ path: path.join(os.tmpdir(), 'bc-e2e', 'smoke-local.png') })
console.log('errors:', errs.filter((e) => !e.includes('404')).slice(0, 5))
await browser.close()
