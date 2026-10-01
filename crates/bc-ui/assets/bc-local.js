// Thin adapter over bc-worklet's BcPlayer for the Rust "this device" session (player/local.rs).
// Served from the app origin together with /player.js, /processor.js and /bc_worklet.wasm.
import { BcPlayer } from '/player.js'

let p = null

export async function init() {
  if (p) return true
  p = await BcPlayer.create({ wasmUrl: '/bc_worklet.wasm', processorUrl: '/processor.js', quality: 1 })
  p.onEvent = (name, data) => window.dispatchEvent(new CustomEvent('bc-local', { detail: { name, data } }))
  return true
}
export const available = () => typeof AudioWorkletNode !== 'undefined' && typeof WebAssembly !== 'undefined'
export const transport = () => (p ? p.transport : 'none')
export const load = (deck, url, startS, rate) => p.load(deck, url, { startS, rate })
export const play = (deck) => p.play(deck)
export const pause = () => p.pause()
export const resume = () => p.resume()
export const stop = () => p.stop()
export const seek = (deck, s) => p.seek(deck, s)
export const volume = (v) => p.setVolume(v)
export const strip = (low, mid, high, filter, echoSend) => p.setStrip({ low, mid, high, filter, echoSend })
export const gapless = (deck) => p.armGapless(deck)
export const transition = (out, inc, kind, lengthS) => p.transition({ out, inc, kind, lengthS })
export const retime = (s) => p.retime(s)
export const cutNow = () => p.cutNow()
export const setEcho = (on) => p.setEcho(on)
export const setSync = (on) => p.setSync(on)
export const nudge = (s) => p.nudge(s)
export const state = () => (p && p.state() ? JSON.stringify(p.state()) : '')
