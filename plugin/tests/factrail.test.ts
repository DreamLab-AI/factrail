// Engine-level tests for the factrail shim. The plugin runs under the real
// engine chain; beneath it the test answers every noun the plugin reads, and
// `process.run` is a fake factrail binary that records each request and
// answers with whatever the test scripts. Run: claude plugin test plugin
//
// The binary is not needed: these prove the wiring of the hook protocol
// (docs/protocol.md) — what is sent, and how each answer, good or bad, is applied.

import { describe, expect, mock, test } from 'claude-code/testing';
import type { On, SessionMessage } from 'claude-code';
import type { MockClock } from 'claude-code/testing';

const SID = 'session-under-test';
const NOW = 1_800_000_000_000;
const SUMMARY: SessionMessage = { role: 'user', text: 'Summary: core compaction ran.', toolUses: [] };

type Call = { argv: readonly string[]; stdin: unknown; timeoutMs: number | undefined };
type Reply = { exitCode?: number; stdout?: string; stderr?: string } | Record<string, unknown> | Error;

type World = {
  calls: Call[];
  /** The plugin's own store, answered beneath it. */
  store: Map<string, unknown>;
  /** What core's own compaction was handed (trigger, instructions). */
  compacts: { trigger: string; instructions?: string }[];
  logs: string[];
  toasts: string[];
  tokens: number;
  messages: SessionMessage[];
  clock: MockClock;
  /** The binary's answer per hook: a response body (wrapped as stdout, exit 0), a raw process result, or a throw. */
  reply: Partial<Record<string, Reply | ((request: Record<string, unknown>) => Reply)>>;
};

function isRaw(r: Reply): r is { exitCode?: number; stdout?: string; stderr?: string } {
  return !(r instanceof Error) && ('stdout' in r || 'exitCode' in r || 'stderr' in r);
}

const ok = (body: Record<string, unknown>) => ({ protocol: 1, ok: true, ...body });

function world(on: On, init: Partial<World> = {}): World {
  const w: World = { calls: [], store: new Map(), compacts: [], logs: [], toasts: [], tokens: 0, messages: [], reply: {}, ...init, clock: mock.clock(on, { now: NOW }) };
  on('store.get', async (_$, e) => ({ value: w.store.get(e.key) }));
  on('store.set', async (_$, e) => { w.store.set(e.key, e.value); return { value: undefined }; });
  on('store.delete', async (_$, e) => { w.store.delete(e.key); return { value: undefined }; });
  on('store.keys', async () => ({ value: [...w.store.keys()] }));
  mock.env(on, {});
  on('session.id', async () => ({ value: SID }));
  on('session.cwd', async () => ({ value: '/work' }));
  on('session.messages', async () => ({ value: w.messages }));
  on('session.usage', async () => ({ value: { context: { tokens: w.tokens, window: 1_000_000, percent: Math.round(w.tokens / 10_000) }, rateLimits: [] } }));
  on('process.run', async (_$, e) => {
    const stdin = e.init?.stdin !== undefined ? JSON.parse(e.init.stdin) as Record<string, unknown> : undefined;
    w.calls.push({ argv: e.argv, stdin, timeoutMs: e.init?.timeoutMs });
    const hook = e.argv[2] ?? '';
    const scripted = w.reply[hook];
    const r = typeof scripted === 'function' ? scripted(stdin ?? {}) : scripted;
    if (r === undefined) return { value: { exitCode: 2, stdout: '', stderr: `no reply scripted for ${hook}` } };
    if (r instanceof Error) throw r;
    if (isRaw(r)) return { value: { exitCode: r.exitCode ?? 0, stdout: r.stdout ?? '', stderr: r.stderr ?? '' } };
    return { value: { exitCode: 0, stdout: JSON.stringify(r), stderr: '' } };
  });
  on('session.compact', async (_$, e) => {
    w.compacts.push({ trigger: e.trigger ?? 'plugin', ...(e.instructions !== undefined ? { instructions: e.instructions } : {}) });
    return { messages: [SUMMARY] };
  });
  on('ui.log', async (_$, e) => { w.logs.push(String((e as { text?: unknown }).text ?? JSON.stringify(e))); return { value: undefined }; });
  on('ui.toast', async (_$, e) => { w.toasts.push(String((e as { text?: unknown }).text ?? JSON.stringify(e))); return { value: undefined }; });
  on('command.register', async () => ({ value: { command: 'factrail' } }));
  on('session.start', async (_$, e) => ({ cwd: e.cwd }) as never);
  on('turn.complete', async () => ({ text: '' }));
  on('turn.start', async (_$, e) => ({ turnId: e.turnId }));
  on('tool.call', async () => ({ result: 'ok' }) as never);
  on('skill.prompt', async (_$, e) => ({ text: e.text }));
  return w;
}

/** Three engine messages with handles and stored results, as session.compact hands them. */
function transcript(): SessionMessage[] {
  return [
    { role: 'user', text: 'List the files.', toolUses: [], handle: 'h0' },
    { role: 'assistant', text: '', handle: 'h1', toolUses: [{ tool_use_id: 'toolu_1', tool: 'Bash', input: { command: 'ls' }, text: 'a\nb', result: { big: 'x'.repeat(1000) } }] },
    { role: 'user', text: '', handle: 'h2', toolUses: [], toolResults: [{ tool_use_id: 'toolu_1', text: 'a\nb', isError: false, result: { big: 'y' } }] },
  ];
}

const compactCalls = (w: World) => w.calls.filter((c) => c.argv[2] === 'compact');
const turnDone = { answer: '', durationMs: 1, isAborted: false, turnId: 'turn', reason: 'answer' as const };

describe('session.compact: install', () => {
  test('keep refs map to the engine\'s own messages; built messages are rebuilt; store writes land', async ($, on) => {
    const w = world(on, { tokens: 190_000 });
    w.reply['compact'] = ok({
      action: 'install',
      messages: [
        { keep: 0 },
        { role: 'assistant', text: 'ran ls', toolUses: [{ tool_use_id: 'toolu_1', tool: 'Bash', input: { command: 'ls' } }] },
        { role: 'user', text: '', toolUses: [], toolResults: [{ tool_use_id: 'toolu_1', text: '[2 lines dropped]' }] },
      ],
      reason: 'ok', log: 'factrail: kept 1/3 messages verbatim', toast: true,
      store: { taint: { tainted: false, at: NOW }, last: 'factrail: kept 1/3' },
    });
    const msgs = transcript();
    const r = await $.session.compact({ trigger: 'manual', messages: msgs } as never);

    // Request: argv, deadline + grace, results stripped, handles kept.
    const [call] = compactCalls(w);
    expect(call?.argv).toEqual(['factrail', 'hook', 'compact']);
    expect(call?.timeoutMs).toBe(25_000);
    const req = call?.stdin as Record<string, any>;
    expect(req.protocol).toBe(1);
    expect(req.session).toEqual({ id: SID, cwd: '/work', trigger: 'manual', agentId: null });
    expect(req.instructions).toBe(null);
    expect(req.usage).toEqual({ tokens: 190_000, window: 1_000_000 });
    expect(req.store).toEqual({ enabled: null, taint: null });
    expect(req.messages[1].handle).toBe('h1');
    expect(req.messages[1].toolUses[0].result).toBe(undefined);
    expect(req.messages[1].toolUses[0].text).toBe('a\nb');
    expect(req.messages[2].toolResults[0].result).toBe(undefined);

    // Response applied.
    expect(w.compacts).toEqual([]);
    expect(r.messages?.length).toBe(3);
    expect(r.messages?.[0]?.handle).toBe('h0');
    expect(r.messages?.[0]?.text).toBe('List the files.');
    expect(r.messages?.[1]?.handle).toBe(undefined);
    expect(r.messages?.[2]?.toolResults?.[0]?.isError).toBe(false);
    expect(await w.store.get(`taint:${SID}`)).toEqual({ tainted: false, at: NOW });
    expect(await w.store.get('last')).toBe('factrail: kept 1/3');
    expect(await w.store.get(`baseline:${SID}`)).toEqual({ pending: true, at: NOW });
    expect(w.logs.some((l) => l.includes('kept 1/3 messages'))).toBe(true);
    expect(w.toasts.length).toBe(1);
  });

  test('the stored switch and taint travel in the request', async ($, on) => {
    const w = world(on);
    w.store.set('enabled', false);
    w.store.set(`taint:${SID}`, { tainted: true, at: NOW });
    w.reply['compact'] = ok({ action: 'builtin', reason: 'switched-off', store: { taint: null, last: null } });
    await $.session.compact({ trigger: 'auto', messages: transcript() } as never);
    expect((compactCalls(w)[0]?.stdin as Record<string, any>).store).toEqual({ enabled: false, taint: { tainted: true, at: NOW } });
  });

  for (const [name, messages] of [
    ['a keep index out of range', [{ keep: 0 }, { keep: 3 }]],
    ['a keep index used twice', [{ keep: 1 }, { keep: 1 }]],
    ['a negative keep index', [{ keep: -1 }]],
    ['a built message with a bad role', [{ keep: 0 }, { role: 'system', text: 'x', toolUses: [] }]],
    ['a built message without toolUses', [{ role: 'user', text: 'x' }]],
    ['a built message with non-string text', [{ role: 'user', text: 7, toolUses: [] }]],
    ['a built message carrying a handle', [{ role: 'user', text: 'x', toolUses: [], handle: 'h1' }]],
    ['a built tool use without input', [{ role: 'assistant', text: '', toolUses: [{ tool_use_id: 't', tool: 'Bash' }] }]],
    ['a non-boolean isError', [{ role: 'user', text: '', toolUses: [], toolResults: [{ tool_use_id: 't', text: '', isError: 'no' }] }]],
    ['an empty list', []],
    ['no list at all', undefined],
  ] as const) {
    test(`malformed install (${name}) falls back to core`, async ($, on) => {
      const w = world(on);
      w.reply['compact'] = ok({ action: 'install', messages, reason: 'ok', store: { taint: null, last: null } });
      const r = await $.session.compact({ trigger: 'manual', messages: transcript() } as never);
      expect(w.compacts).toEqual([{ trigger: 'manual' }]);
      expect(r.messages?.[0]?.text).toBe(SUMMARY.text);
      expect(w.logs.some((l) => l.includes('malformed response'))).toBe(true);
      expect(String(await w.store.get('last'))).toContain('built-in compaction');
    });
  }
});

describe('session.compact: builtin and skip', () => {
  test('builtin with instructions hands core the binary\'s instructions', async ($, on) => {
    const w = world(on);
    w.reply['compact'] = ok({ action: 'builtin', instructions: 'Keep the file list.', reason: 'tainted', log: 'factrail: tainted', toast: false, store: { taint: { tainted: true }, last: 'factrail: tainted' } });
    const r = await $.session.compact({ trigger: 'manual', instructions: 'user words', messages: transcript() } as never);
    expect((compactCalls(w)[0]?.stdin as Record<string, any>).instructions).toBe('user words');
    expect(w.compacts).toEqual([{ trigger: 'manual', instructions: 'Keep the file list.' }]);
    expect(r.messages?.[0]?.text).toBe(SUMMARY.text);
    expect(await w.store.get(`taint:${SID}`)).toEqual({ tainted: true });
    expect(await w.store.get(`baseline:${SID}`)).toEqual({ pending: true, at: NOW });
  });

  test('builtin without instructions passes the event through unchanged', async ($, on) => {
    const w = world(on);
    w.reply['compact'] = ok({ action: 'builtin', reason: 'no-key', store: { taint: null, last: null } });
    await $.session.compact({ trigger: 'auto', instructions: 'user words', messages: transcript() } as never);
    expect(w.compacts).toEqual([{ trigger: 'auto', instructions: 'user words' }]);
  });

  test('skip on a precompute installs nothing, reaches no core, sets no baseline', async ($, on) => {
    const w = world(on);
    w.reply['compact'] = ok({ action: 'skip', reason: 'precompute', store: { taint: null, last: null } });
    const r = await $.session.compact({ trigger: 'precompute', messages: transcript() } as never);
    expect((r as { skip?: string }).skip).toBe('precompute');
    expect(w.compacts).toEqual([]);
    expect(await w.store.get(`baseline:${SID}`)).toBe(undefined);
  });

  test('a subagent\'s compaction carries its agentId and leaves the main baseline alone', async ($, on) => {
    const w = world(on);
    w.reply['compact'] = ok({ action: 'builtin', reason: 'subagent', store: { taint: null, last: null } });
    await $.session.compact({ trigger: 'auto', agentId: 'agent-1', messages: transcript() } as never);
    expect((compactCalls(w)[0]?.stdin as Record<string, any>).session.agentId).toBe('agent-1');
    expect(w.compacts).toEqual([{ trigger: 'auto' }]);
    expect(await w.store.get(`baseline:${SID}`)).toBe(undefined);
  });

  test('the compact process deadline is compactionTimeoutMs plus 10 s', async ($, on) => {
    // Engine tests run with the manifest's defaults; tests/protocol.test.ts varies the option.
    const w = world(on);
    w.reply['compact'] = ok({ action: 'builtin', reason: 'ok', store: {} });
    await $.session.compact({ trigger: 'manual', messages: transcript() } as never);
    expect(compactCalls(w)[0]?.timeoutMs).toBe(15_000 + 10_000);
  });
});

describe('session.compact: every failure is the built-in compaction', () => {
  const failures: [string, Reply, string][] = [
    ['a non-zero exit', { exitCode: 3, stdout: '', stderr: 'thread main panicked\n' }, 'exited 3: thread main panicked'],
    ['unparsable stdout', { stdout: 'not json' }, 'unparsable response'],
    ['a JSON array', { stdout: '[]' }, 'not a JSON object'],
    ['ok: false', { protocol: 1, ok: false, error: 'backend unreachable' }, 'refused: backend unreachable'],
    ['a protocol mismatch', { protocol: 2, ok: true, action: 'install', messages: [{ keep: 0 }] }, 'protocol mismatch'],
    ['a missing protocol', { ok: true, action: 'builtin' }, 'protocol mismatch'],
    ['an unknown action', ok({ action: 'summarise' }), 'unknown action'],
    ['a spawn failure or timeout', new Error('spawn factrail ENOENT'), 'could not run factrail'],
  ];
  for (const [name, reply, why] of failures) {
    test(name, async ($, on) => {
      const w = world(on);
      w.reply['compact'] = reply;
      const r = await $.session.compact({ trigger: 'manual', messages: transcript() } as never);
      expect(w.compacts).toEqual([{ trigger: 'manual' }]);
      expect(r.messages?.[0]?.text).toBe(SUMMARY.text);
      expect(w.logs.some((l) => l.includes(why))).toBe(true);
      // The compaction stood, so the hysteresis baseline resets as for any other.
      expect(await w.store.get(`baseline:${SID}`)).toEqual({ pending: true, at: NOW });
    });
  }
});

describe('turn.complete', () => {
  test('the request carries usage, tools and stored state; compact runs the compaction and marks the baseline pending', async ($, on) => {
    const w = world(on, { tokens: 185_000 });
    w.messages = [
      { role: 'assistant', text: '', toolUses: [
        { tool_use_id: 'a', tool: 'Skill', input: { skill: 'email-search' } },
        { tool_use_id: 'b', tool: 'Skill', input: { name: 'pdf' } },
        { tool_use_id: 'c', tool: 'mcp__x__y', input: {} },
      ] },
    ];
    w.store.set(`baseline:${SID}`, { tokens: 100_000, at: NOW - 1 });
    w.reply['turn'] = ok({ compact: true, reason: 'threshold', threshold: 180_000, need: null, baseline: null, taint: { tainted: true, at: NOW }, nudge: null, log: 'factrail: compacting at 185k' });
    w.reply['compact'] = ok({ action: 'builtin', reason: 'ok', store: { taint: null, last: 'x' } });
    await $.turn.complete(turnDone as never);

    const req = w.calls.find((c) => c.argv[2] === 'turn')?.stdin as Record<string, any>;
    expect(req.protocol).toBe(1);
    expect(req.usage).toEqual({ tokens: 185_000, percent: 19, window: 1_000_000 });
    expect(req.baseline).toEqual({ tokens: 100_000, at: NOW - 1 });
    expect(req.rateLimits).toBe(0);
    expect(req.hasApiKey).toBe(false);
    expect(req.event).toEqual({ reason: 'answer', agentId: null });
    expect(req.tools).toEqual([{ tool: 'Skill', skill: 'email-search' }, { tool: 'Skill', skill: 'pdf' }, { tool: 'mcp__x__y', skill: null }]);
    expect(req.store).toEqual({ taint: null, enabled: null });
    expect(req.now).toBe(NOW);

    expect(w.compacts).toEqual([{ trigger: 'plugin' }]);
    expect(await w.store.get(`taint:${SID}`)).toEqual({ tainted: true, at: NOW });
    expect(await w.store.get(`baseline:${SID}`)).toEqual({ pending: true, at: NOW });
    expect(w.logs).toContain('factrail: compacting at 185k');
  });

  test('a baseline the binary returns is written', async ($, on) => {
    const w = world(on, { tokens: 120_000 });
    w.reply['turn'] = ok({ compact: false, reason: 'below', baseline: { tokens: 120_000, at: NOW }, taint: null, nudge: null, log: null });
    await $.turn.complete(turnDone as never);
    expect(await w.store.get(`baseline:${SID}`)).toEqual({ tokens: 120_000, at: NOW });
    expect(w.compacts).toEqual([]);
  });

  test('a subagent turn never reaches the binary', async ($, on) => {
    const w = world(on);
    await $.turn.complete({ ...turnDone, agentId: 'agent-1' } as never);
    expect(w.calls).toEqual([]);
  });

  test('a failing binary leaves the turn alone', async ($, on) => {
    const w = world(on);
    w.reply['turn'] = { exitCode: 1, stderr: 'boom' };
    await $.turn.complete(turnDone as never);
    expect(w.compacts).toEqual([]);
    expect(w.logs.some((l) => l.includes('turn check failed'))).toBe(true);
  });

  test('two overlapping completions: one consults the binary, the other passes through', async ($, on) => {
    const w = world(on);
    w.reply['turn'] = ok({ compact: false, baseline: null, taint: null, nudge: null, log: null });
    await Promise.all([$.turn.complete(turnDone as never), $.turn.complete(turnDone as never)]);
    expect(w.calls.filter((c) => c.argv[2] === 'turn').length).toBe(1);
    await $.turn.complete(turnDone as never);
    expect(w.calls.filter((c) => c.argv[2] === 'turn').length).toBe(2);
  });
});

describe('cache-warm nudge', () => {
  const nudgeReply = (mode: 'compact' | 'notify') => ok({ compact: false, reason: 'below', baseline: null, taint: null, nudge: { delayMs: 3_300_000, mode, tokens: 120_000 }, log: null });

  test('mode compact compacts when the timer fires on an idle session', async ($, on) => {
    const w = world(on, { tokens: 120_000 });
    w.reply['turn'] = nudgeReply('compact');
    w.reply['compact'] = ok({ action: 'builtin', reason: 'ok', store: {} });
    await $.turn.complete(turnDone as never);
    await w.clock.advance(3_300_000 - 1);
    expect(w.compacts).toEqual([]);
    await w.clock.advance(1);
    expect(w.compacts).toEqual([{ trigger: 'plugin' }]);
    expect(await w.store.get(`baseline:${SID}`)).toEqual({ pending: true, at: NOW + 3_300_000 });
  });

  test('a new turn cancels the pending nudge', async ($, on) => {
    const w = world(on, { tokens: 120_000 });
    w.reply['turn'] = nudgeReply('compact');
    await $.turn.complete(turnDone as never);
    await $.turn.start({ text: 'next', turnId: 't2' });
    await w.clock.advance(4_000_000);
    expect(w.compacts).toEqual([]);
    expect(compactCalls(w)).toEqual([]);
  });

  test('mode notify only toasts', async ($, on) => {
    const w = world(on, { tokens: 120_000 });
    w.reply['turn'] = nudgeReply('notify');
    await $.turn.complete(turnDone as never);
    await w.clock.advance(3_300_000);
    expect(w.compacts).toEqual([]);
    expect(w.toasts.some((t) => t.includes('120k tokens'))).toBe(true);
  });

  test('a malformed nudge is never armed', async ($, on) => {
    const w = world(on, { tokens: 120_000 });
    w.reply['turn'] = ok({ compact: false, baseline: null, taint: null, nudge: { delayMs: 'soon', mode: 'compact' }, log: null });
    await $.turn.complete(turnDone as never);
    await w.clock.advance(4_000_000);
    expect(w.compacts).toEqual([]);
  });
});

describe('/factrail', () => {
  test('status sends args and state, writes the switch the binary returns, shows its text', async ($, on) => {
    const w = world(on, { tokens: 50_000 });
    w.store.set('last', 'factrail: kept 9/10');
    w.reply['status'] = ok({ text: 'factrail: OFF (saved)', store: { enabled: false } });
    const r = await $.command.run({ command: 'factrail', args: 'off' } as never);
    expect(r.text).toBe('factrail: OFF (saved)');
    expect(await w.store.get('enabled')).toBe(false);
    const req = w.calls.find((c) => c.argv[2] === 'status')?.stdin as Record<string, any>;
    expect(req.args).toBe('off');
    expect(req.store).toEqual({ enabled: null, taint: null, last: 'factrail: kept 9/10' });
    expect(req.usage).toEqual({ tokens: 50_000, percent: 5, window: 1_000_000 });
    expect(req.tools).toEqual([]);
    expect('hasKey' in req).toBe(false);
  });

  test('a null switch is not written', async ($, on) => {
    const w = world(on);
    w.store.set('enabled', true);
    w.reply['status'] = ok({ text: 'factrail: ON', store: { enabled: null } });
    await $.command.run({ command: 'factrail', args: 'status' } as never);
    expect(await w.store.get('enabled')).toBe(true);
  });

  test('a failing binary is reported, not hidden', async ($, on) => {
    const w = world(on);
    w.reply['status'] = new Error('spawn factrail ENOENT');
    const r = await $.command.run({ command: 'factrail', args: '' } as never);
    expect(r.text).toContain('the binary failed');
    expect(r.text).toContain('could not run factrail');
  });
});

describe('taint at the source', () => {
  test('tool.call sends the tool and writes the taint the binary returns', async ($, on) => {
    const w = world(on);
    w.reply['taint'] = (req) => ok({ taint: req['tool'] === 'mcp__email-gateway__ask_email' ? { tainted: true, at: NOW } : null });
    const r = await $.tool.call({ tool: 'mcp__email-gateway__ask_email', q: 'invoice' } as never);
    expect((r as { result?: unknown }).result).toBe('ok');
    const req = w.calls[0]?.stdin as Record<string, any>;
    expect(w.calls[0]?.argv).toEqual(['factrail', 'hook', 'taint']);
    expect(w.calls[0]?.timeoutMs).toBe(5_000);
    expect(req).toEqual({ protocol: 1, config: req.config, tool: 'mcp__email-gateway__ask_email', skill: null, store: { taint: null }, now: NOW });
    expect(await w.store.get(`taint:${SID}`)).toEqual({ tainted: true, at: NOW });
  });

  test('a Skill call sends its skill name', async ($, on) => {
    const w = world(on);
    w.reply['taint'] = ok({ taint: null });
    await $.tool.call({ tool: 'Skill', skill: 'email-search' } as never);
    expect((w.calls[0]?.stdin as Record<string, any>).skill).toBe('email-search');
    expect(await w.store.get(`taint:${SID}`)).toBe(undefined);
  });

  test('skill.prompt sends the skill with no tool', async ($, on) => {
    const w = world(on);
    w.reply['taint'] = ok({ taint: { tainted: true, at: NOW } });
    await $.skill.prompt({ skill: 'email-search', text: 'search mail' });
    const req = w.calls[0]?.stdin as Record<string, any>;
    expect(req.tool).toBe(null);
    expect(req.skill).toBe('email-search');
    expect(await w.store.get(`taint:${SID}`)).toEqual({ tainted: true, at: NOW });
  });

  test('a failing binary never blocks the tool call', async ($, on) => {
    const w = world(on);
    w.reply['taint'] = new Error('timed out');
    const r = await $.tool.call({ tool: 'Bash', command: 'ls' } as never);
    expect((r as { result?: unknown }).result).toBe('ok');
    expect(await w.store.get(`taint:${SID}`)).toBe(undefined);
  });
});

describe('session.start', () => {
  test('prunes per-session keys older than 30 days and keeps everything else', async ($, on) => {
    const w = world(on);
    const day = 24 * 60 * 60 * 1000;
    w.store.set('taint:old', { tainted: true, at: NOW - 31 * day });
    w.store.set('baseline:old', { tokens: 1, at: NOW - 31 * day });
    w.store.set('baseline:undated', { tokens: 1 });
    w.store.set('taint:fresh', { tainted: true, at: NOW - day });
    w.store.set('enabled', false);
    await $.session.start({ source: 'startup', cwd: '/work' } as never);
    expect([...w.store.keys()].sort()).toEqual(['enabled', 'taint:fresh']);
  });
});
