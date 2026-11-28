import type { Message } from './messages';

export interface ReceiptSourceLine {
  purchase_order_id: string; purchase_line_id: string; sku: string; revision: string | null;
  unit: 'EA'; ordered_pieces: string;
}

export interface ReceiptAllocation {
  through_receipt_sequence: number;
  receipt_allocated_pieces: string; line_open_pieces: string;
  plan_held_pieces: string; plan_remaining_shortage_pieces: string;
}

export interface ReceiptIntent {
  receipt_id: string; pieces: string;
}

export interface PostedReceipt extends ReceiptIntent {
  posted_at: string; allocation_after: ReceiptAllocation;
}

const object = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);
const matches = (v: unknown, pattern: RegExp): v is string => typeof v === 'string' && pattern.test(v);
export const receiptId = (v: unknown): v is string =>
  matches(v, /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}(?![\s\S])/) && v !== '00000000-0000-0000-0000-000000000000';
export const receiptPieces = (v: unknown): v is string => matches(v, /^(?:[1-9][0-9]{0,5}|1000000)(?![\s\S])/);
const planPieces = (v: unknown): v is string => v === '0' || receiptPieces(v);
export const receiptLineKey = (line: ReceiptSourceLine) => `${line.purchase_order_id}/${line.purchase_line_id}`;

export function receiptSource(body: unknown, plan: string): ReceiptSourceLine[] | null {
  if (!object(body) || body['plan_id'] !== plan || !Array.isArray(body['lines'])
    || body['lines'].length < 1 || body['lines'].length > 32) return null;
  const lines: ReceiptSourceLine[] = [];
  const seen = new Set<string>();
  for (const row of body['lines'] as unknown[]) {
    if (!object(row)) return null;
    const {purchase_order_id, purchase_line_id, sku, revision, unit, ordered_pieces} = row;
    if (!receiptId(purchase_order_id) || !receiptId(purchase_line_id) || !matches(sku, /^[A-Z0-9-]{1,64}(?![\s\S])/)
      || (revision !== null && !matches(revision, /^[A-Za-z0-9._-]{1,32}(?![\s\S])/))
      || unit !== 'EA' || !receiptPieces(ordered_pieces)) return null;
    // Keep only the narrow contract fields even if the server sends something else.
    const line = {purchase_order_id, purchase_line_id, sku, revision, unit, ordered_pieces} as const;
    const key = receiptLineKey(line);
    if (seen.has(key)) return null;
    seen.add(key);
    lines.push(line);
  }
  return lines;
}

export function receiptAllocation(body: unknown, plan: string, source: ReceiptSourceLine): ReceiptAllocation | null {
  if (!object(body)) return null;
  const {plan_id, purchase_order_id, purchase_line_id, sku, revision, unit, through_receipt_sequence,
    receipt_allocated_pieces, line_open_pieces, plan_held_pieces, plan_remaining_shortage_pieces} = body;
  if (plan_id !== plan || purchase_order_id !== source.purchase_order_id || purchase_line_id !== source.purchase_line_id
    || sku !== source.sku || revision !== source.revision || unit !== source.unit
    || typeof through_receipt_sequence !== 'number' || !Number.isInteger(through_receipt_sequence)
    || through_receipt_sequence < 1 || through_receipt_sequence > 1_000_000
    || !receiptPieces(receipt_allocated_pieces) || !planPieces(line_open_pieces)
    || !planPieces(plan_held_pieces) || !planPieces(plan_remaining_shortage_pieces)
    || BigInt(receipt_allocated_pieces) + BigInt(line_open_pieces) !== BigInt(source.ordered_pieces)) return null;
  return {through_receipt_sequence, receipt_allocated_pieces, line_open_pieces, plan_held_pieces, plan_remaining_shortage_pieces};
}

export function postedReceipt(body: unknown, intent: ReceiptIntent, plan: string, line: ReceiptSourceLine): PostedReceipt | null {
  if (!object(body) || body['receipt_id'] !== intent.receipt_id || body['pieces'] !== intent.pieces
    || !matches(body['posted_at'], /^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d{1,24})?(?:[Zz]|[+-]\d{2}:\d{2})(?![\s\S])/)
    || !Number.isFinite(Date.parse(body['posted_at']))) return null;
  const allocation_after = receiptAllocation(body['allocation_after'], plan, line);
  if (!allocation_after || BigInt(allocation_after.receipt_allocated_pieces) < BigInt(intent.pieces)) return null;
  return {...intent, posted_at: body['posted_at'], allocation_after};
}

export function receiptConflict(body: unknown): Message {
  switch (object(body) ? body['code'] : null) {
    case 'over_receipt': return 'receiptOver';
    case 'plan_ineligible': return 'receiptIneligible';
    case 'receipt_intent_changed': return 'receiptChanged';
    default: return 'receiptUnknown';
  }
}
