// SAFETY: refuse to drive a server whose library still has enabled (real) roots.
export async function guard(base) {
  try {
    const r = await (await fetch(base + '/api/library/roots')).json()
    const live = (Array.isArray(r) ? r : []).filter((x) => x.enabled && !/^(\/tmp|\/var\/tmp)\//.test(x.path))
    if (live.length && !process.env.BC_E2E_I_KNOW) {
      console.error(`REFUSING: ${live.length} enabled library root(s) (${live.map((x) => x.path).join(', ')}). Run scripts/safe-db.sh on the DB copy first.`)
      process.exit(3)
    }
  } catch { /* server without the route: nothing to protect */ }
}
