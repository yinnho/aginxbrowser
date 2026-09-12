#!/usr/bin/env node
/**
 * Integration test for the DSH plugin against a live aginxbrowser engine.
 *
 * Requires: `npm install` in this directory, and a local engine running:
 *   AGINXBROWSER_BIND=127.0.0.1:8129 AGINXBROWSER_ALLOW_PRIVATE_NETWORK=1 \
 *     ./target/debug/aginxbrowser
 * Run: npm test   (or AGX_BASE_URL=http://host:port node test/integration.mjs)
 */
import { strict as assert } from 'node:assert'
import { access, rm } from 'node:fs/promises'
import { apply, name, inject } from '../lib/index.js'

const BASE = process.env.AGX_BASE_URL || 'http://127.0.0.1:8129'

let passed = 0
const step = async (label, fn) => {
  try {
    await fn()
    passed++
    console.log(`ok    ${label}`)
  } catch (err) {
    console.error(`FAIL  ${label}`)
    console.error(err && err.stack ? err.stack : err)
    process.exitCode = 1
  }
}

// Minimal mock of the DSH host context: collect registered tools.
const registered = new Map()
const ctx = { tools: { register: (t) => registered.set(t.name, t) }, logger: { info: () => {} } }

apply(ctx, { baseUrl: BASE, screenshotDir: '/tmp/agx-dsh-test' })
const T = (n) => {
  const t = registered.get(n)
  if (!t) throw new Error(`tool not registered: ${n}`)
  return t
}
const run = (n, args) => T(n).execute(args, {})
const expectThrows = async (label, fn, pattern) => {
  let threw = null
  try { await fn() } catch (err) { threw = err }
  if (!threw) throw new Error(`${label}: expected a throw, got a return`)
  if (!pattern.test(String(threw && threw.message))) {
    throw new Error(`${label}: error does not match ${pattern}: ${threw && threw.message}`)
  }
}

const TOOL_COUNT = 15

await step('plugin surface: name/inject/registration count', () => {
  assert.equal(name, 'aginxbrowser')
  assert.ok(inject.includes('tools'))
  assert.equal(registered.size, TOOL_COUNT)
  const missing = [
    'agx_fetch', 'agx_search', 'agx_doctor',
    'agx_session_create', 'agx_session_list', 'agx_session_navigate', 'agx_session_state',
    'agx_session_click', 'agx_session_click_xy', 'agx_session_input', 'agx_session_scroll',
    'agx_session_wait', 'agx_session_eval', 'agx_session_screenshot', 'agx_session_close',
  ].filter((n) => !registered.has(n))
  assert.deepEqual(missing, [])
})

await step('agx_doctor reaches the engine', async () => {
  const v = await run('agx_doctor', {})
  assert.equal(v.ok, true)
})

await step('agx_fetch returns markdown content', async () => {
  const v = await run('agx_fetch', { url: 'https://example.com', max_chars: 2000 })
  assert.equal(v.ok, true)
  assert.match(v.title || '', /Example Domain/i)
  assert.match(v.content || '', /example/i)
})

await step('agx_fetch drops unknown args (engine 400s on them)', async () => {
  const v = await run('agx_fetch', { url: 'https://example.com', nonsense_field: 'x', max_chars: 500 })
  assert.equal(v.ok, true)
})

await step('agx_search returns results', async () => {
  const v = await run('agx_search', { q: 'rust programming language', max_results: 5 })
  assert.equal(v.ok, true)
  assert.ok(Array.isArray(v.results) && v.results.length > 0, 'no results')
  assert.ok(v.results[0].url, 'result missing url')
})

let sid = null
await step('agx_session_create returns id + live_view_url', async () => {
  const v = await run('agx_session_create', { url: 'https://example.com', width: 800, height: 600 })
  assert.equal(v.ok, true)
  assert.ok(v.session_id, 'no session_id')
  assert.match(v.live_view_url, new RegExp(`/live\\.html\\?session=`))
  sid = v.session_id
})

await step('agx_session_wait selector settles', async () => {
  const v = await run('agx_session_wait', { session_id: sid, selector: 'h1' })
  assert.equal(v.ok, true)
  assert.equal(v.matched, true)
})

await step('agx_session_state returns indexed text', async () => {
  const v = await run('agx_session_state', { session_id: sid })
  assert.equal(v.ok, true)
  assert.match(v.text, /url=/)
  assert.match(v.text, /viewport=/)
})

await step('agx_session_eval computes and reads back', async () => {
  const v = await run('agx_session_eval', { session_id: sid, script: '6 * 7' })
  assert.equal(v.ok, true)
  assert.equal(v.result, 42)
})

let inputIndex = null
await step('eval can inject an input, state indexes it', async () => {
  await run('agx_session_eval', {
    session_id: sid,
    script: `const i = document.createElement('input'); i.id = 'dsh_probe';
             i.setAttribute('placeholder', 'probe'); document.body.appendChild(i); 'ok'`,
  })
  const v = await run('agx_session_state', { session_id: sid })
  const m = v.text.match(/\[(\d+)\]\s+<input/g) || []
  assert.ok(m.length > 0, `state listing has no input element:\n${v.text.slice(0, 500)}`)
  inputIndex = Number(m[0].match(/\[(\d+)\]/)[1])
})

await step('agx_session_input types, eval verifies', async () => {
  const v = await run('agx_session_input', { session_id: sid, index: inputIndex, text: 'hello dsh' })
  assert.equal(v.ok, true)
  const back = await run('agx_session_eval', { session_id: sid, script: `document.getElementById('dsh_probe').value` })
  assert.equal(back.result, 'hello dsh')
})

let linkIndex = null
await step('state exposes the example.com link for clicking', async () => {
  const v = await run('agx_session_state', { session_id: sid })
  const m = v.text.match(/\[(\d+)\]\s+<a\b/)
  assert.ok(m, 'state listing has no <a> element')
  linkIndex = Number(m[1])
})

await step('agx_session_click by index', async () => {
  const v = await run('agx_session_click', { session_id: sid, index: linkIndex })
  assert.equal(v.ok, true)
  assert.notEqual(v.clicked, false)
})

await step('agx_session_scroll', async () => {
  const v = await run('agx_session_scroll', { session_id: sid, direction: 'down', amount: 1 })
  assert.equal(v.ok, true)
})

let shotPath = null
await step('agx_session_screenshot saves a real file', async () => {
  const v = await run('agx_session_screenshot', { session_id: sid })
  assert.equal(v.ok, true)
  assert.match(v.path, /\.png$/)
  assert.ok(Number(v.width) > 0 && Number(v.height) > 0)
  await access(v.path)
  shotPath = v.path
})

await step('agx_session_list sees the session', async () => {
  const v = await run('agx_session_list', {})
  assert.equal(v.ok, true)
  assert.ok(Array.isArray(v.sessions))
  assert.ok(v.sessions.some((s) => (s.session_id || s.id) === sid))
})

await step('agx_session_close', async () => {
  const v = await run('agx_session_close', { session_id: sid })
  assert.equal(v.ok, true)
})

await step('closed session errors with HTTP status, not a crash', async () => {
  await expectThrows('state on closed session', () => run('agx_session_state', { session_id: sid }), /HTTP 4\d\d/)
})

await step('bogus session id errors cleanly', async () => {
  await expectThrows('state on bogus id', () => run('agx_session_state', { session_id: 's_nope' }), /HTTP 4\d\d/)
})

if (shotPath) await rm(shotPath, { force: true })

console.log(`\n${passed} steps passed${process.exitCode ? ' (with failures above)' : ''} against ${BASE}`)
