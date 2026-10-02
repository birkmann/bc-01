// Interaction smoke: command palette, scoped shortcuts, theme persistence, table sort/selection/column drag.
import { chromium } from 'playwright-core'
const base = process.argv[2] ?? 'http://127.0.0.1:8420'
import { guard } from './guard.mjs'
await guard(base)
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome-stable', args: ['--no-sandbox'] })
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
const fails = []
const check = (name, ok) => { console.log(ok ? 'PASS' : 'FAIL', name); if (!ok) fails.push(name) }
page.on('pageerror', (e) => { fails.push('pageerror ' + e.message); console.log('pageerror', e.message) })
await page.goto(base + '/tracks', { waitUntil: 'networkidle' })
await page.waitForSelector('.dt-row')

// palette
await page.keyboard.press('Control+k')
await page.waitForSelector('[aria-label="Command palette"]')
await page.keyboard.type('albums')
await page.waitForTimeout(300)
await page.keyboard.press('Enter')
await page.waitForURL('**/albums')
check('palette navigates to a page', page.url().endsWith('/albums'))

// scoped shortcuts: q toggles the planner outside inputs...
await page.keyboard.press('q')
await page.waitForTimeout(300)
check('q opens the planner', await page.locator('.right-panel.open').count() === 1)
await page.keyboard.press('q')
await page.waitForTimeout(200)
check('q closes the planner', await page.locator('.right-panel.open').count() === 0)
// ...but never while typing in an input
await page.goto(base + '/tracks', { waitUntil: 'networkidle' })
await page.locator('.topbar input[type=search]').click()
await page.keyboard.type('qs v')
check('shortcuts ignored inside an input', await page.locator('.right-panel.open').count() === 0 && await page.locator('.deck').count() === 0)
const val = await page.locator('.topbar input[type=search]').inputValue()
check('typing reaches the input', val === 'qs v')
await page.locator('.topbar input[type=search]').fill('')
await page.locator('body').click({ position: { x: 700, y: 400 } })
// "/" focuses search
await page.keyboard.press('/')
check('/ focuses the search', await page.evaluate(() => document.activeElement?.type === 'search'))
await page.keyboard.press('Escape')
await page.locator('body').click({ position: { x: 700, y: 400 } })
// deck
await page.keyboard.press('v')
await page.waitForTimeout(300)
check('v opens the deck view', await page.locator('.deck').count() === 1)
await page.keyboard.press('Escape')
await page.waitForTimeout(200)
check('Escape closes the deck view', await page.locator('.deck').count() === 0)

// theme toggle persists server-side
const before = await page.evaluate(() => document.documentElement.dataset.theme)
await page.locator('button[title="Toggle light / dark"]').first().click()
await page.waitForTimeout(1200)
const after = await page.evaluate(() => document.documentElement.dataset.theme)
check('theme toggles', before !== after)
const saved = await page.evaluate(async () => (await fetch('/api/ui-state/themes')).json())
check('theme stored server-side (ui_state)', saved && saved.mode === after)
await page.locator('button[title="Toggle light / dark"]').first().click()
await page.waitForTimeout(800)

// table: header sort sends server-side sort; selection with shift range
await page.goto(base + '/tracks', { waitUntil: 'networkidle' })
await page.waitForSelector('.dt-row')
const reqs = []
page.on('request', (r) => { if (r.url().includes('/api/tracks?')) reqs.push(r.url()) })
await page.locator('.dt-th', { hasText: 'Title' }).first().click()
await page.waitForTimeout(800)
check('sort goes to the server', reqs.some((u) => u.includes('sort=title')))
await page.locator('.dt-row .dt-check').nth(1).click()
await page.locator('.dt-row .dt-check').nth(5).click({ modifiers: ['Shift'] })
await page.waitForTimeout(200)
const n = await page.locator('.dt-row.sel').count()
check('shift-range selects 5 rows', n === 5)

// table: drag a header's right edge to resize, drag the header to move the column
const hdrag = async (sel, dx) => {
  const b = await page.locator(sel).boundingBox()
  const x = b.x + b.width / 2, y = b.y + b.height / 2
  await page.mouse.move(x, y)
  await page.mouse.down()
  for (let i = 1; i <= 10; i++) await page.mouse.move(x + (dx * i) / 10, y)
  await page.mouse.up()
  await page.waitForTimeout(250)
}
const colW = (id) => page.$eval(`.dt-th[data-col="${id}"]`, (e) => e.getBoundingClientRect().width)
const bpmW = await colW('bpm')
await hdrag('.dt-th[data-col="bpm"] .dt-resize', 50)
check('header edge resizes the column', Math.abs((await colW('bpm')) - bpmW - 50) <= 2)
const cols = () => page.$$eval('.dt-th[data-col]', (els) => els.map((e) => e.dataset.col))
const tb = await page.locator('.dt-th[data-col="duration"]').boundingBox()
const ab = await page.locator('.dt-th[data-col="artist"]').boundingBox()
await hdrag('.dt-th[data-col="duration"]', ab.x + 10 - (tb.x + tb.width / 2))
const order = await cols()
check('dragging a header moves the column', order.indexOf('duration') === order.indexOf('artist') - 1)
await page.locator('.dt-cols button').click()
await page.getByText('Reset widths and order').click()
await page.waitForTimeout(250)
check('column layout resets', (await cols()).indexOf('duration') > (await cols()).indexOf('artist'))
console.log(fails.length ? `${fails.length} FAILED` : 'all interaction checks passed')
await browser.close()
process.exit(fails.length ? 1 : 0)
