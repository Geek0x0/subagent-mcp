import type { EngineInterface, Register } from 'claude-code'

const workerTools = /^(?:mcp__worker__|mcp__plugin_subagent_|mcp__subagent__)/
const offContext = 'The worker delegation policy is OFF for this turn (set by /worker off): do not dispatch to the worker, edit directly; the policy\'s baseline and review rules still apply; a later turn without this note means the policy is ON again.'
const sonnetContext = "Worker mode is sonnet: dispatch write work with the Agent tool to a general-purpose subagent with model: 'sonnet', put the constraints listed under 'The dispatch' in the worker policy at the top of the dispatch prompt, continue the same subagent with SendMessage, do not use the MCP worker tools; a later turn without this note means mode is mcp."
const policyIntro = (name: string) => `These are the user's standing worker-delegation instructions, delivered by the ${name} plugin, to be followed for the rest of the conversation.`

const statePath = async ($: EngineInterface) => {
  const home = await $.env.get('HOME')
  if (!home) throw new Error('HOME is not set')
  return `${home}/.claude/worker-policy.state`
}
const isOff = async ($: EngineInterface) => {
  const path = await statePath($)
  return (await $.fs.read(path).catch(() => '')).trim() === 'off'
}
const modePath = async ($: EngineInterface) => {
  const home = await $.env.get('HOME')
  if (!home) throw new Error('HOME is not set')
  return `${home}/.claude/worker-policy.mode`
}
const readMode = async ($: EngineInterface) => {
  const path = await modePath($)
  return (await $.fs.read(path).catch(() => '')).trim() === 'sonnet' ? 'sonnet' : 'mcp'
}
const refresh = ($: EngineInterface, off: boolean, mode: string) => $.ui.status(off ? 'worker: off' : mode === 'sonnet' ? 'worker: sonnet' : undefined)

export const register: Register = (on, options) => {
  let policy: Promise<string> | undefined
  let generation = 0
  let delivered = -1

  on('session.compact', async ($, e, next) => {
    const result = await next(e)
    if (!e.agentId && e.trigger !== 'precompute' && result.skip === undefined) generation++
    return result
  }).catch(($, e, next) => next(e))

  on('session.end', ($, e, next) => {
    generation++
    return next(e)
  }).catch(($, e, next) => next(e))

  on('session.start', async ($, e, next) => {
    await $.command.register({ name: 'worker', description: 'Set the worker delegation switch.' })
    refresh($, await isOff($), await readMode($))
    return next(e)
  }).catch(($, e, next) => next(e))

  on('command.run', { command: 'worker' }, async ($, e) => {
    const arg = e.args.trim().toLowerCase()
    if (arg === 'off' || arg === 'on') await $.fs.write(await statePath($), arg)
    const modeArg = /^mode\s+(mcp|sonnet)$/.exec(arg)?.[1]
    if (modeArg) await $.fs.write(await modePath($), modeArg)
    const off = await isOff($)
    const mode = await readMode($)
    refresh($, off, mode)
    const state = off ? 'OFF' : 'ON'
    return { text: `Worker policy: ${state}, mode: ${mode}`, context: [`Worker policy: ${state}, mode: ${mode}; applies from now.`] }
  }).catch(($, e, next) => next(e))

  on('prompt.submit', async ($, e, next) => {
    const off = await isOff($)
    const mode = await readMode($)
    refresh($, off, mode)
    const current = generation
    let policyContext: string | undefined
    if (delivered !== current) {
      policy ??= $.fs.read(`${$.plugin.root}/policy/WORKER.md`)
      policyContext = `${policyIntro($.plugin.name)}\n${await policy}`
    }
    const context = [...(e.context ?? [])]
    if (policyContext) context.push(policyContext)
    if (off) context.push(offContext)
    else if (mode === 'sonnet') context.push(sonnetContext)
    const result = await next(context.length ? { ...e, context } : e)
    if (policyContext && result.context?.includes(policyContext)) delivered = current
    return result
  }).catch(($, e, next) => next(e))

  on('tool.check', { tool: workerTools }, async ($, e, next) => {
    if (await isOff($)) return { decision: 'deny', reason: 'The worker switch is off (/worker on re-enables it); edit directly.' }
    if (await readMode($) === 'sonnet') return { decision: 'deny', reason: 'Worker mode is sonnet; dispatch to a general-purpose subagent with model "sonnet" instead. /worker mode mcp switches back.' }
    return next(e)
  }).catch(($, e, next) => next(e))
}
