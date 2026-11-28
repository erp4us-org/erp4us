import { readBoundedJson } from './bounded-json';
import type { Message } from './messages';

export const LOG_LIMIT = 100;
export const POLL_MS = 6000;
const PREVIEW_BODY_LIMIT = 64 * 1024;

const SERVICE_LABELS = {
  'master-data': 'diagnosticsMasterData',
  manufacturing: 'manufacturing',
  'article-relay': 'diagnosticsArticleRelay',
} as const satisfies Record<string, Message>;
const RUNTIME_STATES = {
  present_running: 'runtimeRunning',
  present_exited: 'runtimeExited',
  absent: 'runtimeAbsent',
} as const satisfies Record<string, Message>;
const SEVERITIES = {
  info: 'logsInfo',
  warning: 'logsWarning',
  error: 'logsError',
} as const satisfies Record<string, Message>;
const LOG_CODES = {
  manufacturing: { plan_read_unavailable: 'logsPlanReadUnavailable' },
  'article-relay': {
    relay_connected: 'logsRelayConnected',
    relay_delivery_delayed: 'logsRelayDeliveryDelayed',
  },
} as const satisfies Record<string, Record<string, Message>>;

export type RuntimeReport = { state: 'unavailable' }
  | { state: 'stale'; observedAt: number }
  | { state: 'fresh'; observedAt: number; expiresAt: number; rows: { service: Message; status: Message }[] };
export interface LogRow {
  observedAt: number;
  service: Message;
  severity: Message;
  code: Message;
}
interface LogPage {
  rows: LogRow[];
  before: string | null;
  after: string;
  gap: boolean;
}

const object = (value: unknown): Record<string, unknown> | null =>
  value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : null;
const timestamp = (value: unknown): value is number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 && value <= 253402300799999;
const cursorToken = (value: unknown): value is string =>
  typeof value === 'string' && value.length > 0 && value.length <= 256 && !/[^A-Za-z0-9_-]/.test(value);
const lookup = <T>(values: Readonly<Record<string, T>>, key: unknown): T | undefined =>
  typeof key === 'string' && Object.hasOwn(values, key) ? values[key] : undefined;

export function runtimeReport(value: unknown, now: number): RuntimeReport {
  const report = object(value);
  const observedAt = report?.['observed_at_unix_ms'];
  const expiresAt = report?.['expires_at_unix_ms'];
  if (!timestamp(observedAt)) return { state: 'unavailable' };
  if (report?.['state'] === 'stale' && expiresAt === null && report['services'] === null) {
    return { state: 'stale', observedAt };
  }
  if (report?.['state'] !== 'fresh' || !timestamp(expiresAt)
      || expiresAt <= observedAt || expiresAt - observedAt > 30000) return { state: 'unavailable' };
  const services = object(report['services']);
  const labels = Object.entries(SERVICE_LABELS);
  if (!services || Object.keys(services).length !== labels.length) return { state: 'unavailable' };
  const rows: { service: Message; status: Message }[] = [];
  for (const [name, service] of labels) {
    const status = lookup(RUNTIME_STATES, services[name]);
    if (!status) return { state: 'unavailable' };
    rows.push({ service, status });
  }
  if (now < observedAt || now >= expiresAt) return { state: 'stale', observedAt };
  return { state: 'fresh', observedAt, expiresAt, rows };
}

export function logPage(value: unknown): LogPage | null {
  const report = object(value);
  if (report?.['state'] !== 'available') return null;
  const page = object(report['page']);
  if (!page) return null;
  const { records, before_cursor: before, after_cursor: after, gap_before: gap } = page;
  if (!Array.isArray(records) || records.length > LOG_LIMIT || (before !== null && !cursorToken(before))
      || !cursorToken(after) || typeof gap !== 'boolean') return null;
  const rows: LogRow[] = [];
  for (const value of records) {
    const record = object(value);
    if (!record) continue;
    const { observed_at_unix_ms: observedAt, service, severity, code } = record;
    const label = lookup(SERVICE_LABELS, service);
    const level = lookup(SEVERITIES, severity);
    const codeLabels = lookup<Readonly<Record<string, Message>>>(LOG_CODES, service);
    const message = codeLabels && lookup(codeLabels, code);
    if (timestamp(observedAt) && label && level && message) {
      // Never keep or render incidental fields from a nonconforming source.
      rows.push({ observedAt, service: label, severity: level, code: message });
    }
  }
  return { rows, before, after, gap };
}

type LogPath = `/v1/admin/diagnostic-logs?${string}`;

export function logPath(cursor: string | null): LogPath {
  const query = new URLSearchParams({ limit: String(LOG_LIMIT) });
  if (cursor !== null) query.set('cursor', cursor);
  return `/v1/admin/diagnostic-logs?${query}`;
}

type PreviewResponse = { kind: 'ok'; body: unknown } | { kind: 'denied' | 'gap' | 'unavailable' };

export async function readPreview(
  path: '/v1/admin/runtime' | LogPath, headers: { Authorization: string }, signal: AbortSignal,
): Promise<PreviewResponse> {
  try {
    const response = await fetch(path, {
      headers, signal: AbortSignal.any([signal, AbortSignal.timeout(5000)]),
      cache: 'no-store', credentials: 'omit', redirect: 'error',
    });
    if (response.ok) {
      const body = await readBoundedJson(response, PREVIEW_BODY_LIMIT);
      return { kind: 'ok', body };
    }
    await response.body?.cancel();
    if (response.status === 401 || response.status === 403) return { kind: 'denied' };
    if (response.status === 410) return { kind: 'gap' };
  } catch {
    // No response bodies or upstream errors are exposed to the UI.
  }
  return { kind: 'unavailable' };
}
