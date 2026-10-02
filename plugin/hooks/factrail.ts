// factrail.ts — the Claude Code function-hook shim over the factrail binary.
//
// Every policy and compaction decision lives in the binary. For each hook this
// module runs `<binary> hook <name>` once through `$.process.run`, writes one
// JSON request to its stdin, reads one JSON response from its stdout and
// applies it (docs/protocol.md, version 1). What stays here is engine plumbing
// the binary cannot do from outside the process:
//   • the store writes the binary asks for (`taint:`, `baseline:`, `enabled`, `last`);
//   • the `/factrail` command registration;
//   • the turn generation counter, the cache-warm timer and the guards that keep
//     two overlapping turn completions, or a timer and a turn, from compacting twice;
//   • the 30-day pruning of the plugin's own per-session store keys (housekeeping
//     of this plugin's store, not policy; the binary only ever sees one session's
//     entries, so it cannot prune the rest);
//   • validation of every response, so a malformed answer can never reach the
//     session: any failure means the built-in behaviour and one log line.

import type { EngineInterface, On, PluginOptions, Register, SessionCompactInput, SessionCompactResult, SessionMessage, Timer, TurnCompleteInput } from 'claude-code';

import {
  TIMEOUT_MS,
  baselineKey,
  binaryPath,
  callBinary,
  compactEventFields,
  compactRequest,
  compactTimeoutMs,
  contextUsage,
  expiredSessionKeys,
  readCompact,
  readStatus,
  readTaint,
  readTurn,
  skillOf,
  statusRequest,
  taintKey,
  taintRequest,
  toolRefs,
  turnRequest,
} from './protocol.js';
import type { Body, CallResult, HookName } from './protocol.js';

const COMMAND = 'factrail';
const STORE_ENABLED = 'enabled';
const STORE_LAST = 'last';
const TOAST_MS = 12_000;
const NUDGE_TOAST_MS = 60_000;

type Engine = EngineInterface;

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** A stored value as the request carries it: absent ⇒ null. */
function stored(v: unknown): unknown {
  return v === undefined ? null : v;
}

/** What the hooks share: the guards and the cache-warm timer state. */
type State = { compacting: boolean; evaluating: boolean };

/** `<binary> hook <name>` through the host; never throws (see callBinary). */
async function runHook($: Engine, binary: string, hook: HookName, request: Body, timeoutMs: number): Promise<CallResult> {
  return callBinary((argv, init) => $.process.run(argv, init), binary, hook, request, timeoutMs);
}

/** Runs one compaction from inside the plugin; the session.compact hook asks the binary how. */
async function compactNow($: Engine, state: State, why: string): Promise<void> {
  if (state.compacting) return;
  state.compacting = true;
  try {
    const r = await $.session.compact();
    if (r && 'skip' in r && r.skip) {
      $.ui.log(`factrail: ${why} compaction skipped (${r.skip})`);
      return;
    }
    // The compaction stood, so the next turn's size is the new hysteresis baseline.
    const sessionId = await $.session.id();
    if (sessionId) await $.store.set(baselineKey(sessionId), { pending: true, at: await $.clock.now() });
  } finally {
    state.compacting = false;
  }
}

/** `factrail hook taint` for one tool call or skill expansion; never throws. */
async function markTaint($: Engine, options: PluginOptions, binary: string, tool: string | null, skill: string | null): Promise<void> {
  try {
    const sessionId = await $.session.id();
    if (!sessionId) return;
    const request = taintRequest({ config: options, tool, skill, store: { taint: stored(await $.store.get(taintKey(sessionId))) }, now: await $.clock.now() });
    const r = await runHook($, binary, 'taint', request, TIMEOUT_MS.taint);
    if (!r.ok) { $.ui.log(`factrail: taint check failed (${r.why})`); return; }
    const value = readTaint(r.body);
    if (value !== undefined) await $.store.set(taintKey(sessionId), value);
  } catch { /* marking must never block a tool call; the turn and compact hooks see the transcript too */ }
}

export const register: Register = (on: On, options: PluginOptions) => {
  const binary = binaryPath(options);
  // `compacting`: a compaction this plugin started is in flight. `evaluating`: one
  // turn.complete is consulting the binary (claimed synchronously, before any await).
  const state: State = { compacting: false, evaluating: false };
  // Cache-warm nudge: one pending timer, cancelled by any turn starting. `generation`
  // makes a timer that fires after a newer turn began a no-op even if cancel raced it.
  let nudge: Timer | undefined;
  let generation = 0;
  let turnRunning = false;
  const cancelNudge = () => { nudge?.cancel(); nudge = undefined; };

  on('session.start', async ($, event, next) => {
    try {
      await $.command.register({
        name: COMMAND,
        description: 'factrail verbatim compaction: on | off | status.',
        argumentHint: 'on|off|status',
      });
    } catch { /* an older engine without command.register still compacts; only the switch is lost */ }
    try {
      const now = await $.clock.now();
      const keys = (await $.store.keys()).filter((k) => /^(taint|baseline):/.test(k));
      const entries: [string, unknown][] = [];
      for (const k of keys) entries.push([k, await $.store.get(k)]);
      for (const k of expiredSessionKeys(entries, now)) await $.store.delete(k);
    } catch { /* pruning is housekeeping; never block a session on it */ }
    return next(event);
  });

  // Taint at the source, before any content reaches the transcript.
  on('tool.call', async ($, event, next) => {
    const input = event as unknown as Record<string, unknown>;
    await markTaint($, options, binary, String(event.tool), skillOf(String(event.tool), input));
    return next(event);
  });

  on('skill.prompt', async ($, event, next) => {
    await markTaint($, options, binary, null, event.skill);
    return next(event);
  });

  on('command.run', { command: COMMAND }, async ($, event) => {
    try {
      const sessionId = await $.session.id();
      const { context } = await $.session.usage();
      const request = statusRequest({
        config: options,
        args: event.args,
        store: {
          enabled: stored(await $.store.get(STORE_ENABLED)),
          taint: sessionId ? stored(await $.store.get(taintKey(sessionId))) : null,
          last: stored(await $.store.get(STORE_LAST)),
        },
        usage: contextUsage(context),
        baseline: sessionId ? stored(await $.store.get(baselineKey(sessionId))) : null,
        tools: toolRefs(await $.session.messages()),
      });
      const r = await runHook($, binary, 'status', request, TIMEOUT_MS.status);
      if (!r.ok) return { text: `factrail: the binary failed (${r.why}); compaction falls back to the built-in summary until it answers` };
      const status = readStatus(r.body);
      if ('ok' in status) return { text: `factrail: the binary failed (${status.why}); compaction falls back to the built-in summary until it answers` };
      if (status.enabled !== undefined) await $.store.set(STORE_ENABLED, status.enabled);
      return { text: status.text };
    } catch (error) {
      return { text: `factrail: status failed (${message(error)})` };
    }
  });

  on('session.compact', async ($, event: SessionCompactInput, next): Promise<SessionCompactResult> => {
    let sessionId: string | undefined;
    // Any compaction that stands on the main conversation resets the hysteresis
    // baseline; its true size is read by the binary at the next turn. A subagent's
    // own compaction leaves the main context, and so the baseline, alone.
    const settle = async (result: SessionCompactResult): Promise<SessionCompactResult> => {
      try {
        if (sessionId && !event.agentId && result && !('skip' in result && result.skip)) {
          await $.store.set(baselineKey(sessionId), { pending: true, at: await $.clock.now() });
        }
      } catch { /* the baseline is an optimisation; the compaction stands either way */ }
      return result;
    };
    const fallBack = async (why: string): Promise<SessionCompactResult> => {
      const note = `factrail: built-in compaction (${why})`;
      try { $.ui.log(note); await $.store.set(STORE_LAST, note); } catch { /* logging only */ }
      return settle(await next(event));
    };

    let outcome;
    try {
      sessionId = await $.session.id();
      const fields = compactEventFields(event);
      const { context } = await $.session.usage();
      const request = compactRequest({
        config: options,
        session: { id: sessionId ?? null, cwd: await $.session.cwd(), trigger: fields.trigger, agentId: fields.agentId },
        instructions: fields.instructions,
        messages: event.messages,
        usage: { tokens: context.tokens ?? null, window: context.window ?? null },
        store: {
          enabled: stored(await $.store.get(STORE_ENABLED)),
          taint: sessionId ? stored(await $.store.get(taintKey(sessionId))) : null,
        },
      });
      const r = await runHook($, binary, 'compact', request, compactTimeoutMs(options));
      if (!r.ok) return fallBack(r.why);
      const read = readCompact(event.messages, r.body);
      if (!read.ok) return fallBack(`malformed response: ${read.why}`);
      outcome = read.outcome;
    } catch (error) {
      return fallBack(message(error));
    }

    try {
      if (sessionId && outcome.taint !== undefined) await $.store.set(taintKey(sessionId), outcome.taint);
      if (outcome.last !== undefined) await $.store.set(STORE_LAST, outcome.last);
      if (outcome.log) $.ui.log(outcome.log);
      if (outcome.toast) $.ui.toast(outcome.log ?? `factrail: ${outcome.reason ?? outcome.plan.action}`, { timeoutMs: TOAST_MS });
    } catch { /* the writes are bookkeeping; the decision below still applies */ }

    const plan = outcome.plan;
    if (plan.action === 'skip') return { skip: plan.reason };
    if (plan.action === 'install') return settle({ messages: plan.messages });
    return settle(await next(plan.instructions !== undefined ? { ...event, instructions: plan.instructions } : event));
  });

  on('turn.start', async (_$, event, next) => {
    generation += 1; turnRunning = true; cancelNudge();
    return next(event);
  });

  on('turn.complete', async ($, event: TurnCompleteInput, next) => {
    // Subagent turns carry their own loop; the usage and the triggers are the main loop's.
    if (event.agentId) return next(event);
    turnRunning = false;
    cancelNudge();
    // Claimed BEFORE the first await, so two overlapping dispatches cannot both get
    // past it, and released only by its claimant.
    if (state.compacting || state.evaluating) return next(event);
    state.evaluating = true;
    try {
      const sessionId = await $.session.id();
      const usage = await $.session.usage();
      const messages: readonly SessionMessage[] = await $.session.messages();
      const request = turnRequest({
        config: options,
        usage: contextUsage(usage.context),
        baseline: sessionId ? stored(await $.store.get(baselineKey(sessionId))) : null,
        rateLimits: usage.rateLimits.length,
        hasApiKey: Boolean(await $.env.get('ANTHROPIC_API_KEY')),
        event: { reason: (event as { reason?: string }).reason ?? null, agentId: null },
        tools: toolRefs(messages),
        store: {
          taint: sessionId ? stored(await $.store.get(taintKey(sessionId))) : null,
          enabled: stored(await $.store.get(STORE_ENABLED)),
        },
        now: await $.clock.now(),
      });
      const r = await runHook($, binary, 'turn', request, TIMEOUT_MS.turn);
      if (!r.ok) {
        $.ui.log(`factrail: turn check failed (${r.why})`);
        return next(event);
      }
      const turn = readTurn(r.body);
      if (sessionId && turn.baseline !== undefined) await $.store.set(baselineKey(sessionId), turn.baseline);
      if (sessionId && turn.taint !== undefined) await $.store.set(taintKey(sessionId), turn.taint);
      if (turn.log) $.ui.log(turn.log);
      if (turn.compact) {
        await compactNow($, state, 'threshold');
        return next(event);
      }
      if (turn.nudge) {
        const { delayMs, mode, tokens } = turn.nudge;
        const armedAt = generation;
        nudge = $.clock.after(delayMs, () => {
          nudge = undefined;
          if (armedAt !== generation || turnRunning || state.compacting) return;
          const size = tokens !== null ? `${Math.round(tokens / 1000)}k tokens` : 'a large context';
          if (mode === 'notify') {
            $.ui.toast(`factrail: ${size} in context and the prompt cache expires soon — /compact now while it is warm`, { timeoutMs: NUDGE_TOAST_MS });
            return;
          }
          $.ui.log(`factrail: idle at ${size} — compacting before the prompt cache expires`);
          compactNow($, state, 'cache-warm').catch((error: unknown) => {
            $.ui.log(`factrail: cache-warm compaction skipped (${message(error)})`);
          });
        });
      }
    } catch (error) {
      $.ui.log(`factrail: auto-compact skipped (${message(error)})`);
    } finally {
      state.evaluating = false;
    }
    return next(event);
  });
};
