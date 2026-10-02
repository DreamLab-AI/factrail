// protocol.ts — the plugin's side of the factrail hook protocol, version 1
// (docs/protocol.md). Pure: no engine calls. Everything here takes the process
// runner as an argument, so it is testable without Claude Code.
//
// The plugin holds no policy. It builds one JSON request per hook, runs the
// binary once, checks the response's envelope and shape, and hands back
// something the hook can apply verbatim. Anything it cannot check is a failure,
// and a failure always means the built-in behaviour.

import type { PluginOptions, ProcessRunInit, ProcessRunResult, SessionCompactInput, SessionMessage } from 'claude-code';

export const PROTOCOL = 1;

/** The hooks the binary serves, as `factrail hook <name>` spells them. */
export type HookName = 'compact' | 'turn' | 'taint' | 'status';

/** `$.process.run`, or a test's stand-in. */
export type Runner = (argv: readonly string[], init?: ProcessRunInit) => Promise<ProcessRunResult>;

/** A JSON object the binary answered with, envelope checked. */
export type Body = Record<string, unknown>;

export type CallResult = { ok: true; body: Body } | { ok: false; why: string };

/** Longest stretch of stderr or an error message quoted in a log line. */
const QUOTE_CHARS = 200;

/** `$.process.run` refuses longer than ten minutes. */
const MAX_TIMEOUT_MS = 600_000;

/** Default backend deadline when `compactionTimeoutMs` is absent or not positive (matches the binary). */
export const DEFAULT_COMPACTION_TIMEOUT_MS = 15_000;

/** Grace the binary gets past its own deadline to fall back and answer. */
export const COMPACT_GRACE_MS = 10_000;

/**
 * Per-hook process timeouts. Only `compact` talks to a model; the other three
 * are local decisions over a small request, so a stuck binary must not hold a
 * tool call or a turn for the engine's 30 s default.
 */
export const TIMEOUT_MS: Readonly<Record<Exclude<HookName, 'compact'>, number>> = {
  taint: 5_000,
  turn: 10_000,
  status: 10_000,
};

/** The binary from the `binary` option; a bare name is resolved on PATH by the host. */
export function binaryPath(options: PluginOptions): string {
  const v = options['binary'];
  return typeof v === 'string' && v.trim().length > 0 ? v.trim() : 'factrail';
}

/**
 * The process timeout for `factrail hook compact`: the binary's own deadline
 * (resolved exactly as the binary resolves it) plus a grace to fall back in.
 */
export function compactTimeoutMs(options: PluginOptions): number {
  const v = options['compactionTimeoutMs'];
  const n = typeof v === 'number' ? v : typeof v === 'string' && v.trim() !== '' ? Number(v) : NaN;
  const deadline = Number.isFinite(n) && n > 0 ? Math.ceil(n) : DEFAULT_COMPACTION_TIMEOUT_MS;
  return Math.min(deadline + COMPACT_GRACE_MS, MAX_TIMEOUT_MS);
}

function quote(text: string): string {
  const line = text.trim().split('\n').filter(Boolean).pop() ?? '';
  return line.length > QUOTE_CHARS ? `${line.slice(0, QUOTE_CHARS)}…` : line;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/**
 * Runs `<binary> hook <name>` once with `request` on stdin and checks the
 * envelope: exit 0, one JSON object, `protocol: 1`, `ok: true`. Never throws.
 */
export async function callBinary(run: Runner, binary: string, hook: HookName, request: Body, timeoutMs: number): Promise<CallResult> {
  let out: ProcessRunResult;
  try {
    out = await run([binary, 'hook', hook], { stdin: JSON.stringify(request), timeoutMs });
  } catch (error) {
    return { ok: false, why: `could not run ${binary}: ${quote(error instanceof Error ? error.message : String(error))}` };
  }
  let parsed: unknown;
  let parseError: string | undefined;
  try {
    parsed = JSON.parse(out.stdout);
  } catch (error) {
    parseError = error instanceof Error ? error.message : String(error);
  }
  if (out.exitCode !== 0) {
    const said = isObject(parsed) && typeof parsed['error'] === 'string' ? parsed['error'] : quote(out.stderr);
    return { ok: false, why: `${binary} exited ${out.exitCode}${said ? `: ${quote(said)}` : ''}` };
  }
  if (parseError !== undefined) return { ok: false, why: `unparsable response (${quote(parseError)})` };
  if (!isObject(parsed)) return { ok: false, why: 'response is not a JSON object' };
  if (parsed['protocol'] !== PROTOCOL) return { ok: false, why: `protocol mismatch (got ${JSON.stringify(parsed['protocol'] ?? null)}, want ${PROTOCOL})` };
  if (parsed['ok'] !== true) {
    const error = typeof parsed['error'] === 'string' ? parsed['error'] : 'no error given';
    return { ok: false, why: `refused: ${quote(error)}` };
  }
  return { ok: true, body: parsed };
}

// ── requests ──────────────────────────────────────────────────────────────

/** What the taint rule reads of one tool call. */
export type ToolRef = { tool: string; skill: string | null };

/** `input.skill ?? input.name` of a `Skill` call, else null. */
export function skillOf(tool: string, input: Record<string, unknown> | undefined): string | null {
  if (tool !== 'Skill' || !input) return null;
  const v = input['skill'] ?? input['name'];
  return typeof v === 'string' ? v : null;
}

/** The session's tool calls reduced to `{ tool, skill }`, in transcript order. */
export function toolRefs(messages: readonly SessionMessage[]): ToolRef[] {
  const out: ToolRef[] = [];
  for (const m of messages) for (const t of m.toolUses) out.push({ tool: t.tool, skill: skillOf(t.tool, t.input) });
  return out;
}

/** A message without `toolUses[].result` / `toolResults[].result` (the stored records); the handle stays. */
export function stripMessage(m: SessionMessage): SessionMessage {
  const out: SessionMessage = {
    role: m.role,
    text: m.text,
    toolUses: m.toolUses.map(({ result: _result, ...rest }) => rest),
  };
  if (m.toolResults) out.toolResults = m.toolResults.map(({ result: _result, ...rest }) => rest);
  if (m.handle !== undefined) out.handle = m.handle;
  return out;
}

export type ContextUsage = { tokens: number | null; percent: number | null; window: number | null };

export function contextUsage(context: { tokens?: number; percent?: number; window?: number } | undefined): ContextUsage {
  return { tokens: context?.tokens ?? null, percent: context?.percent ?? null, window: context?.window ?? null };
}

export function compactRequest(args: {
  config: PluginOptions;
  session: { id: string | null; cwd: string | null; trigger: string; agentId: string | null };
  instructions: string | null;
  messages: readonly SessionMessage[];
  usage: { tokens: number | null; window: number | null };
  store: { enabled: unknown; taint: unknown };
}): Body {
  return {
    protocol: PROTOCOL,
    config: args.config,
    session: args.session,
    instructions: args.instructions,
    messages: args.messages.map(stripMessage),
    usage: args.usage,
    store: args.store,
  };
}

export function turnRequest(args: {
  config: PluginOptions;
  usage: ContextUsage;
  baseline: unknown;
  rateLimits: number;
  hasApiKey: boolean;
  event: { reason: string | null; agentId: string | null };
  tools: ToolRef[];
  store: { taint: unknown; enabled: unknown };
  now: number;
}): Body {
  return { protocol: PROTOCOL, ...args };
}

export function taintRequest(args: { config: PluginOptions; tool: string | null; skill: string | null; store: { taint: unknown }; now: number }): Body {
  return { protocol: PROTOCOL, ...args };
}

export function statusRequest(args: {
  config: PluginOptions;
  args: string;
  store: { enabled: unknown; taint: unknown; last: unknown };
  usage: ContextUsage;
  baseline: unknown;
  tools: ToolRef[];
}): Body {
  return { protocol: PROTOCOL, ...args };
}

// ── responses ─────────────────────────────────────────────────────────────

/** Absent, null or a non-string ⇒ null; the binary's line otherwise. */
export function lineOf(v: unknown): string | null {
  return typeof v === 'string' && v.length > 0 ? v : null;
}

/** `store` of a response, or an empty record. */
function storeOf(body: Body): Record<string, unknown> {
  const s = body['store'];
  return isObject(s) ? s : {};
}

/** A store write the plugin must make: present and not null. `undefined` means "write nothing". */
function writeOf(v: unknown): unknown {
  return v === null ? undefined : v;
}

export type CompactPlan =
  | { action: 'install'; messages: SessionMessage[] }
  | { action: 'builtin'; instructions?: string }
  | { action: 'skip'; reason: string };

export type CompactOutcome = {
  plan: CompactPlan;
  reason: string | null;
  log: string | null;
  toast: boolean;
  /** Value for `taint:<session id>`; undefined ⇒ no write. */
  taint: unknown;
  /** Value for `last`; undefined ⇒ no write. */
  last: unknown;
};

/** One built message checked and rebuilt field by field, or why it is malformed. */
function builtMessage(raw: Record<string, unknown>, at: number): SessionMessage | string {
  if ('handle' in raw && raw['handle'] !== undefined) return `messages[${at}] is built but carries a handle`;
  const role = raw['role'];
  if (role !== 'user' && role !== 'assistant') return `messages[${at}].role is not user or assistant`;
  if (typeof raw['text'] !== 'string') return `messages[${at}].text is not a string`;
  const uses = raw['toolUses'];
  if (!Array.isArray(uses)) return `messages[${at}].toolUses is not an array`;
  const toolUses: SessionMessage['toolUses'] = [];
  for (const [j, u] of uses.entries()) {
    if (!isObject(u) || typeof u['tool_use_id'] !== 'string' || typeof u['tool'] !== 'string' || !isObject(u['input'])) {
      return `messages[${at}].toolUses[${j}] lacks tool_use_id, tool or input`;
    }
    if (u['text'] !== undefined && u['text'] !== null && typeof u['text'] !== 'string') return `messages[${at}].toolUses[${j}].text is not a string`;
    toolUses.push({
      tool_use_id: u['tool_use_id'],
      tool: u['tool'],
      input: u['input'],
      ...(typeof u['text'] === 'string' ? { text: u['text'] } : {}),
      ...(u['isError'] === true ? { isError: true as const } : {}),
    });
  }
  const message: SessionMessage = { role, text: raw['text'], toolUses };
  const results = raw['toolResults'];
  if (results !== undefined && results !== null) {
    if (!Array.isArray(results)) return `messages[${at}].toolResults is not an array`;
    const toolResults: NonNullable<SessionMessage['toolResults']> = [];
    for (const [j, r] of results.entries()) {
      if (!isObject(r) || typeof r['tool_use_id'] !== 'string' || typeof r['text'] !== 'string') {
        return `messages[${at}].toolResults[${j}] lacks tool_use_id or text`;
      }
      if (r['isError'] !== undefined && r['isError'] !== null && typeof r['isError'] !== 'boolean') {
        return `messages[${at}].toolResults[${j}].isError is not a boolean`;
      }
      toolResults.push({ tool_use_id: r['tool_use_id'], text: r['text'], isError: r['isError'] === true });
    }
    if (toolResults.length > 0) message.toolResults = toolResults;
  }
  return message;
}

/**
 * An `install` message list mapped back onto the engine's own messages:
 * `{ keep: i }` is `input[i]` itself (handle intact), anything else is a built
 * message rebuilt from its checked fields. Any defect rejects the whole list,
 * because a half-applied list would corrupt the session.
 */
export function mapInstall(input: readonly SessionMessage[], raw: unknown): { ok: true; messages: SessionMessage[] } | { ok: false; why: string } {
  if (!Array.isArray(raw)) return { ok: false, why: 'install without a messages array' };
  if (raw.length === 0) return { ok: false, why: 'install with an empty message list' };
  const used = new Set<number>();
  const out: SessionMessage[] = [];
  for (const [at, entry] of raw.entries()) {
    if (!isObject(entry)) return { ok: false, why: `messages[${at}] is not an object` };
    if ('keep' in entry) {
      const i = entry['keep'];
      if (typeof i !== 'number' || !Number.isInteger(i) || i < 0 || i >= input.length) {
        return { ok: false, why: `messages[${at}].keep ${JSON.stringify(i)} is not an index into ${input.length} messages` };
      }
      if (used.has(i)) return { ok: false, why: `messages[${at}].keep ${i} is used twice` };
      used.add(i);
      out.push(input[i] as SessionMessage);
      continue;
    }
    const built = builtMessage(entry, at);
    if (typeof built === 'string') return { ok: false, why: built };
    out.push(built);
  }
  return { ok: true, messages: out };
}

/** A `compact` response read into what the hook applies, or why it cannot be. */
export function readCompact(input: readonly SessionMessage[], body: Body): { ok: true; outcome: CompactOutcome } | { ok: false; why: string } {
  const store = storeOf(body);
  const common = {
    reason: lineOf(body['reason']),
    log: lineOf(body['log']),
    toast: body['toast'] === true,
    taint: writeOf(store['taint']),
    last: writeOf(store['last']),
  };
  const action = body['action'];
  if (action === 'install') {
    const mapped = mapInstall(input, body['messages']);
    if (!mapped.ok) return mapped;
    return { ok: true, outcome: { ...common, plan: { action: 'install', messages: mapped.messages } } };
  }
  if (action === 'builtin') {
    const instructions = body['instructions'];
    if (instructions !== undefined && instructions !== null && typeof instructions !== 'string') {
      return { ok: false, why: 'builtin with non-string instructions' };
    }
    return { ok: true, outcome: { ...common, plan: typeof instructions === 'string' ? { action: 'builtin', instructions } : { action: 'builtin' } } };
  }
  if (action === 'skip') {
    return { ok: true, outcome: { ...common, plan: { action: 'skip', reason: lineOf(body['reason']) ?? 'factrail: skipped' } } };
  }
  return { ok: false, why: `unknown action ${JSON.stringify(action ?? null)}` };
}

export type Nudge = { delayMs: number; mode: 'compact' | 'notify'; tokens: number | null };

export type TurnOutcome = {
  compact: boolean;
  baseline: unknown;
  taint: unknown;
  nudge: Nudge | null;
  log: string | null;
};

/** A `turn` response; a malformed `nudge` is dropped (never armed), the rest still applies. */
export function readTurn(body: Body): TurnOutcome {
  let nudge: Nudge | null = null;
  const n = body['nudge'];
  if (isObject(n) && typeof n['delayMs'] === 'number' && Number.isFinite(n['delayMs']) && n['delayMs'] >= 0 &&
      (n['mode'] === 'compact' || n['mode'] === 'notify')) {
    nudge = { delayMs: n['delayMs'], mode: n['mode'], tokens: typeof n['tokens'] === 'number' ? n['tokens'] : null };
  }
  return {
    compact: body['compact'] === true,
    baseline: writeOf(body['baseline']),
    taint: writeOf(body['taint']),
    nudge,
    log: lineOf(body['log']),
  };
}

/** A `taint` response: the value to write, or undefined. */
export function readTaint(body: Body): unknown {
  return writeOf(body['taint']);
}

/** A `status` response: the text to show and the switch to persist (boolean only). */
export function readStatus(body: Body): { text: string; enabled: boolean | undefined } | { ok: false; why: string } {
  const text = body['text'];
  if (typeof text !== 'string') return { ok: false, why: 'status without text' };
  const enabled = storeOf(body)['enabled'];
  return { text, enabled: typeof enabled === 'boolean' ? enabled : undefined };
}

// ── housekeeping ──────────────────────────────────────────────────────────

/** Per-session store entries untouched this long are pruned at session start. */
export const SESSION_STATE_TTL_MS = 30 * 24 * 60 * 60 * 1000;

export const taintKey = (sessionId: string) => `taint:${sessionId}`;
export const baselineKey = (sessionId: string) => `baseline:${sessionId}`;

/**
 * Keys of `taint:`/`baseline:` entries whose `at` is older than the TTL or
 * missing. The plugin's own store hygiene, not policy: the binary never sees
 * the store as a whole, only the session's own entries.
 */
export function expiredSessionKeys(entries: readonly (readonly [string, unknown])[], now: number, ttlMs = SESSION_STATE_TTL_MS): string[] {
  const out: string[] = [];
  for (const [key, value] of entries) {
    if (!/^(taint|baseline):/.test(key)) continue;
    const at = isObject(value) ? Number(value['at']) : NaN;
    if (!Number.isFinite(at) || now - at > ttlMs) out.push(key);
  }
  return out;
}

/** The compact request's `session.trigger`/`agentId`/`instructions` from the engine's event. */
export function compactEventFields(event: SessionCompactInput): { trigger: string; agentId: string | null; instructions: string | null } {
  return { trigger: event.trigger, agentId: event.agentId ?? null, instructions: event.instructions ?? null };
}
