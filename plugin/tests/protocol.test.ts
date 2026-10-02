// Unit tests for hooks/protocol.ts, the pure half of the shim: option reading
// the engine tests cannot vary (they run with the manifest's defaults), and the
// identity guarantee of a `keep` reference. Run: claude plugin test plugin

import { describe, expect, test } from 'claude-code/testing';
import type { SessionMessage } from 'claude-code';

import { binaryPath, callBinary, compactTimeoutMs, expiredSessionKeys, mapInstall, stripMessage } from '../hooks/protocol.js';

describe('options', () => {
  test('compactTimeoutMs is the binary deadline plus 10 s, resolved as the binary resolves it', () => {
    expect(compactTimeoutMs({ compactionTimeoutMs: 2_500 })).toBe(12_500);
    expect(compactTimeoutMs({ compactionTimeoutMs: '4000' })).toBe(14_000);
    expect(compactTimeoutMs({ compactionTimeoutMs: 0 })).toBe(25_000);
    expect(compactTimeoutMs({ compactionTimeoutMs: -1 })).toBe(25_000);
    expect(compactTimeoutMs({})).toBe(25_000);
    expect(compactTimeoutMs({ compactionTimeoutMs: 10_000_000 })).toBe(600_000);
  });

  test('binaryPath defaults to factrail on PATH', () => {
    expect(binaryPath({})).toBe('factrail');
    expect(binaryPath({ binary: '  ' })).toBe('factrail');
    expect(binaryPath({ binary: '/opt/agentbox/bin/factrail' })).toBe('/opt/agentbox/bin/factrail');
  });
});

describe('callBinary', () => {
  test('an ok:false answer on a non-zero exit quotes the binary\'s error, not stderr', async () => {
    const r = await callBinary(async () => ({ exitCode: 1, stdout: '{"protocol":1,"ok":false,"error":"bad request"}', stderr: 'noise' }), 'factrail', 'turn', {}, 1_000);
    expect(r).toEqual({ ok: false, why: 'factrail exited 1: bad request' });
  });

  test('argv and stdin are exactly one request', async () => {
    let seen: { argv: readonly string[]; stdin?: string; timeoutMs?: number } | undefined;
    await callBinary(async (argv, init) => { seen = { argv, stdin: init?.stdin, timeoutMs: init?.timeoutMs }; return { exitCode: 0, stdout: '{"protocol":1,"ok":true}', stderr: '' }; }, 'fr', 'status', { protocol: 1 }, 7);
    expect(seen).toEqual({ argv: ['fr', 'hook', 'status'], stdin: '{"protocol":1}', timeoutMs: 7 });
  });
});

describe('install mapping', () => {
  test('a keep reference is the very object the engine handed over', () => {
    const input: SessionMessage[] = [{ role: 'user', text: 'a', toolUses: [], handle: 'h0' }];
    const r = mapInstall(input, [{ keep: 0 }]);
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.messages[0]).toBe(input[0]);
  });

  test('stripMessage drops stored results and keeps the handle', () => {
    const m = stripMessage({ role: 'assistant', text: '', handle: 'h', toolUses: [{ tool_use_id: 't', tool: 'Bash', input: {}, result: 1, isError: true }], toolResults: [{ tool_use_id: 't', text: 'x', isError: false, result: 2 }] });
    expect(m).toEqual({ role: 'assistant', text: '', handle: 'h', toolUses: [{ tool_use_id: 't', tool: 'Bash', input: {}, isError: true }], toolResults: [{ tool_use_id: 't', text: 'x', isError: false }] });
  });
});

describe('store pruning', () => {
  test('only taint:/baseline: keys past 30 days, or without a date, expire', () => {
    const now = 100 * 86_400_000;
    const keys = expiredSessionKeys([
      ['taint:a', { at: now - 31 * 86_400_000 }],
      ['taint:b', { at: now - 29 * 86_400_000 }],
      ['baseline:c', { tokens: 1 }],
      ['baseline:d', 'junk'],
      ['last', { at: 0 }],
    ], now);
    expect(keys).toEqual(['taint:a', 'baseline:c', 'baseline:d']);
  });
});
