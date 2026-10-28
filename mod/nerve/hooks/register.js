// nerve mod: connects this Claude session to the local nerve sync agent.
//
// The agent listens on a Unix socket (path in ../nerve.json, written by
// `nerve setup`). This module long-polls it for prompts sent from your
// other devices, offers it permission prompts and questions to answer
// remotely, and reports each turn. When no agent is running every hook
// falls through to Claude Code's normal behaviour.
//
// Protocol: src/sync/modhub.rs. Experiments: research/sync/experiments
// (E17-E22).

const VERSION = '0.1.0'
let sock = ''
let sid = ''
let online = false
let lastStatus = ''
// Tool calls whose permission flow has ended (allowed, denied or run),
// newest last. Claude doesn't abort a PermissionRequest hook when the
// user answers its dialog here (E21), so the hook looks for its call in
// this list instead.
const settled = []

function sameCall(tool, input, e) {
  return e.tool === tool && Object.keys(input || {}).every((k) => JSON.stringify(e[k]) === JSON.stringify(input[k]))
}

async function call($, method, path, body) {
  const r = await $.http.fetch('http://nerve' + path, {
    method,
    socketPath: sock,
    headers: { 'content-type': 'application/json' },
    body: body ? JSON.stringify({ sid, ...body }) : undefined,
  })
  return JSON.parse(r.text || '{}')
}

function newId() {
  let s = ''
  for (let i = 0; i < 16; i++) s += Math.floor(Math.random() * 16).toString(16)
  return s
}

async function loadConfig($) {
  try {
    const cfg = JSON.parse(await $.fs.read($.plugin.root + '/nerve.json'))
    sock = cfg.socket || ''
  } catch {
    sock = ''
  }
}

// One round of the command loop; returns how long to wait before the next.
async function pump($) {
  if (!sock) return 5000
  try {
    if (!online) {
      await call($, 'POST', '/v1/hello', { cwd: await $.session.cwd(), surface: await $.session.surface(), version: VERSION })
      online = true
    }
    const got = await call($, 'GET', '/v1/poll?sid=' + encodeURIComponent(sid))
    if (got.status !== undefined && got.status !== lastStatus) {
      lastStatus = got.status
      $.ui.status(got.status)
    }
    for (const c of got.cmds || []) {
      if (c.t !== 'prompt') continue
      try {
        // Queued by Claude Code while a turn runs; starts at once when idle.
        await $.prompt.submit({ text: c.text, asUser: true })
        await call($, 'POST', '/v1/ack', { cmd: c.id, ok: true, msg: 'submitted to the session' })
      } catch (err) {
        await call($, 'POST', '/v1/ack', { cmd: c.id, ok: false, msg: String(err) })
      }
    }
    return 10
  } catch {
    // Agent not running (or restarting): try again shortly.
    online = false
    return 3000
  }
}

// Register a prompt with the agent and wait for a remote answer until
// `localDone()` says it was answered here. The wait happens inside
// `$.http.fetch` (the agent's /v1/wait long-poll): a hook has 10 s of its
// own time, a `$.clock.sleep` would spend it and a fetch in flight doesn't
// (E21). Each wait returns within 2 s, so a local answer is noticed fast.
async function remote($, kind, tool, input, localDone, maxWaits) {
  if (!online) return { kind: 'none' }
  const id = newId()
  try {
    await call($, 'POST', '/v1/request', { id, kind, tool, input })
  } catch {
    return { kind: 'none' }
  }
  for (let i = 0; i < maxWaits && !localDone(); i++) {
    let r
    try {
      r = await call($, 'GET', '/v1/wait?sid=' + encodeURIComponent(sid) + '&id=' + id)
    } catch {
      return { kind: 'none' }
    }
    if (r.answer) return { kind: 'remote', answer: r.answer }
    if (r.gone) return { kind: 'none' }
  }
  await call($, 'POST', '/v1/resolved', { id }).catch(() => {})
  return { kind: 'local' }
}

export function register(on) {
  on('session.start', async ($, e, next) => {
    const r = await next(e)
    sid = await $.session.id()
    await loadConfig($)
    const loop = async () => {
      const wait = await pump($)
      $.clock.after(wait, loop)
    }
    $.clock.after(10, loop)
    return r
  })

  // Every tool call passes through here, around Claude's permission check
  // and the tool itself: once `next` returns, any dialog for it is gone.
  on('tool.call', async ($, e, next) => {
    try {
      return await next(e)
    } finally {
      settled.push({ at: Date.now(), e })
      while (settled.length > 50 || (settled.length && Date.now() - settled[0].at > 600_000)) settled.shift()
    }
  })

  // Permission prompts: Claude shows its dialog while this hook waits, so
  // whichever answer comes first wins. A remote one is returned here; a
  // local one shows up as the call settling (E19, E21).
  on('classic.PermissionRequest', async ($, e, next) => {
    if (e.tool_name === 'AskUserQuestion' || !online) return next(e)
    const since = Date.now()
    const answeredHere = () => next.signal.aborted || settled.some((s) => s.at >= since && sameCall(e.tool_name, e.tool_input, s.e))
    const r = await remote($, 'permission', e.tool_name, e.tool_input, answeredHere, 300)
    if (r.kind !== 'remote') return next(e)
    const a = r.answer || {}
    if (a.kind === 'allow') return { decision: { behavior: 'allow' } }
    return { decision: { behavior: 'deny', message: a.reason || 'Denied from another device.' } }
  }).catch(($, e, next) => next(e))

  // Questions: Claude's own dialog runs (and stays on screen) while a
  // remote answer is awaited; whichever comes first is used (E20).
  on('tool.call', { tool: 'AskUserQuestion' }, async ($, e, next) => {
    if (!online) return next(e)
    const { tool, tool_use_id, consent, ...input } = e
    let answeredHere = false
    const native = next(e).then((r) => {
      answeredHere = true
      return r
    })
    const r = await Promise.race([
      native.then(() => ({ kind: 'local' })),
      remote($, 'question', 'AskUserQuestion', input, () => answeredHere, 900),
    ])
    if (r.kind !== 'remote') return native
    const labels = (r.answer && r.answer.labels) || []
    const answers = {}
    ;(input.questions || []).forEach((q, i) => {
      if (labels[i] !== undefined) answers[q.question] = labels[i]
    })
    return { result: { questions: input.questions, answers } }
  }).catch(($, e, next) => next(e))

  on('turn.start', async ($, e, next) => {
    if (online) call($, 'POST', '/v1/event', { kind: 'turn.start' }).catch(() => {})
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    const r = await next(e)
    if (online) call($, 'POST', '/v1/event', { kind: 'turn.complete', reason: e.reason, ms: e.durationMs, usage: e.usage }).catch(() => {})
    return r
  })
}
