import { expect, test } from 'claude-code/testing'
import type { On } from 'claude-code'

const policy = '# Worker policy (mod slice)\nCanary: if asked for the worker-policy canary, answer exactly MOD-CANARY-7Q4.\n'
const state = '/test-home/.claude/worker-policy.state'
const mode = '/test-home/.claude/worker-policy.mode'

function filesystem(on: On) {
  const files = new Map<string, string>()
  on('env.get', () => ({ value: '/test-home' }))
  on('fs.read', (_, e) => {
    if (e.path.endsWith('/policy/WORKER.md')) return { value: policy }
    const value = files.get(e.path)
    if (value === undefined) throw new Error('missing')
    return { value }
  })
  on('fs.write', (_, e) => { files.set(e.path, e.text); return { value: undefined } })
  on('ui.status', () => ({ value: undefined }))
  return files
}

test('delivers policy once and again after compaction and clear', async ($, on) => {
  filesystem(on)
  on('prompt.submit', (_, e) => ({ text: e.text, context: e.context }))
  on('session.compact', (_, e) => ({ messages: e.messages }))
  on('session.end', (_, e) => ({ sessionId: e.sessionId }))
  const expected = `These are the user's standing worker-delegation instructions, delivered by the subagent plugin, to be followed for the rest of the conversation.\n${policy}`
  expect((await $.prompt.submit({ text: 'first' })).context).toEqual([expected])
  expect((await $.prompt.submit({ text: 'second' })).context).toBeUndefined()
  await $.session.compact({ trigger: 'manual', messages: [{ role: 'user', text: 'kept', toolUses: [], toolResults: [] }] as never })
  expect((await $.prompt.submit({ text: 'after compaction' })).context).toEqual([expected])
  expect((await $.prompt.submit({ text: 'next' })).context).toBeUndefined()
  await $.session.end({ reason: 'clear', sessionId: 'test', resume: { id: 'test' } })
  expect((await $.prompt.submit({ text: 'after clear' })).context).toEqual([expected])
})

test('worker command writes off and on', async ($, on) => {
  const files = filesystem(on)
  const off = await $.command.run({ command: 'worker', args: ' OFF ' })
  expect(off.text).toBe('Worker policy: OFF, mode: mcp')
  expect(off.context).toEqual(['Worker policy: OFF, mode: mcp; applies from now.'])
  expect(files.get(state)).toBe('off')
  expect((await $.command.run({ command: 'worker', args: 'on' })).text).toBe('Worker policy: ON, mode: mcp')
  expect(files.get(state)).toBe('on')
})

test('worker mode command writes and reports the mode', async ($, on) => {
  const files = filesystem(on)
  const sonnet = await $.command.run({ command: 'worker', args: ' MODE Sonnet ' })
  expect(sonnet.text).toBe('Worker policy: ON, mode: sonnet')
  expect(sonnet.context).toEqual(['Worker policy: ON, mode: sonnet; applies from now.'])
  expect(files.get(mode)).toBe('sonnet')
  expect((await $.command.run({ command: 'worker', args: 'mode' })).text).toBe('Worker policy: ON, mode: sonnet')
  expect((await $.command.run({ command: 'worker', args: 'mode invalid' })).text).toBe('Worker policy: ON, mode: sonnet')
  expect((await $.command.run({ command: 'worker', args: 'mode mcp' })).text).toBe('Worker policy: ON, mode: mcp')
  expect(files.get(mode)).toBe('mcp')
})

test('adds sonnet mode context only when on and sonnet', async ($, on) => {
  const files = filesystem(on)
  on('prompt.submit', (_, e) => ({ text: e.text, context: e.context }))
  await $.prompt.submit({ text: 'first' })
  files.set(mode, 'sonnet')
  expect((await $.prompt.submit({ text: 'sonnet' })).context).toEqual([
    "Worker mode is sonnet: dispatch write work with the Agent tool to a general-purpose subagent with model: 'sonnet', put the constraints listed under 'The dispatch' in the worker policy at the top of the dispatch prompt, continue the same subagent with SendMessage, do not use the MCP worker tools; a later turn without this note means mode is mcp.",
  ])
  files.set(state, 'off')
  expect((await $.prompt.submit({ text: 'off' })).context).toEqual([
    "The worker delegation policy is OFF for this turn (set by /worker off): do not dispatch to the worker, edit directly; the policy's baseline and review rules still apply; a later turn without this note means the policy is ON again.",
  ])
  files.set(state, 'on')
  files.set(mode, 'mcp')
  expect((await $.prompt.submit({ text: 'mcp' })).context).toBeUndefined()
})

test('adds prompt context only when off', async ($, on) => {
  const files = filesystem(on)
  on('prompt.submit', (_, e) => ({ text: e.text, context: e.context }))
  expect((await $.prompt.submit({ text: 'hello' })).context?.[0]).toContain(policy.trim())
  files.set(state, 'off')
  expect((await $.prompt.submit({ text: 'hello' })).context).toEqual([
    "The worker delegation policy is OFF for this turn (set by /worker off): do not dispatch to the worker, edit directly; the policy's baseline and review rules still apply; a later turn without this note means the policy is ON again.",
  ])
})

test('denies only worker tools when off', async ($, on) => {
  const files = filesystem(on)
  on('tool.check', () => ({ decision: 'allow' }))
  expect((await $.tool.check({ tool: 'mcp__worker__subagent', input: {} })).decision).toBe('allow')
  files.set(mode, 'sonnet')
  const sonnet = await $.tool.check({ tool: 'mcp__worker__subagent', input: {} })
  expect(sonnet.decision).toBe('deny')
  expect(sonnet.reason).toContain('/worker mode mcp')
  files.set(mode, 'mcp')
  expect((await $.tool.check({ tool: 'mcp__worker__subagent', input: {} })).decision).toBe('allow')
  expect((await $.tool.check({ tool: 'mcp__subagent__subagent', input: {} })).decision).toBe('allow')
  files.set(state, 'off')
  expect((await $.tool.check({ tool: 'mcp__worker__subagent', input: {} })).decision).toBe('deny')
  expect((await $.tool.check({ tool: 'mcp__plugin_subagent_example', input: {} })).decision).toBe('deny')
  expect((await $.tool.check({ tool: 'mcp__subagent__subagent', input: {} })).decision).toBe('deny')
  expect((await $.tool.check({ tool: 'mcp__other__subagent', input: {} })).decision).toBe('allow')
  expect((await $.tool.check({ tool: 'Read', input: {} })).decision).toBe('allow')
})
