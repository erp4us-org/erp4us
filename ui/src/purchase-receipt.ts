import {
  afterNextRender, Component, computed, effect, ElementRef, inject, Injectable, Injector, input, linkedSignal, signal, viewChild,
} from '@angular/core';
import { LucideCheck, LucideClipboardList, LucideRefreshCw, LucideSearch, LucideX } from '@lucide/angular';
import { ArticleApi } from './article-api';
import { readBoundedJson } from './bounded-json';
import type { Message } from './messages';
import { Preferences } from './preferences';
import {
  postedReceipt, receiptAllocation, receiptConflict, receiptId, receiptLineKey, receiptPieces, receiptSource,
  type PostedReceipt, type ReceiptAllocation, type ReceiptIntent, type ReceiptSourceLine,
} from './purchase-receipt-api';
import { Feedback } from './ui';

const MAX_RECEIPT_RESPONSE_BYTES = 16 * 1024;

interface Access {
  plan: string; sourcePath: string; allocationBase: string; key: string;
  headers: Record<string, string>; control: AbortController;
}
interface PendingReceipt {
  line: ReceiptSourceLine; intent: ReceiptIntent; needsOperator?: boolean;
}
interface View {
  lines: readonly ReceiptSourceLine[]; selected: ReceiptSourceLine | null;
  draft: string; review: string | null; denied: boolean;
  busy: 'source' | 'read' | 'post' | null; feedback: Message | null;
  current: ReceiptAllocation | null; readFeedback: Message;
  posted: PostedReceipt | null;
}
const empty = (): View => ({
  lines: [], selected: null, draft: '', review: null, denied: false, busy: null, feedback: null,
  current: null, readFeedback: 'receiptNotChecked', posted: null,
});

// Retain only an unresolved lot's redacted line and exact intent. The command
// survives route changes in the same verified session, not sign-out or reload.
@Injectable({providedIn: 'root'})
class ReceiptCommands {
  private readonly api = inject(ArticleApi);
  readonly session = computed(() => {
    this.api.credentialGeneration();
    return signal<ReadonlyMap<string, PendingReceipt>>(new Map());
  });
}

@Component({
  selector: 'erp-receipt-allocation',
  template: `
    @let row = allocation();
    <dl>
      <dt>{{ t()('receiptLineOpen') }}:</dt><dd>{{ row.line_open_pieces }} EA</dd>
      <dt>{{ t()('receiptPlanHeld') }}:</dt><dd>{{ row.plan_held_pieces }} EA</dd>
      <dt>{{ t()('receiptPlanShortage') }}:</dt><dd>{{ row.plan_remaining_shortage_pieces }} EA</dd>
      <dt>{{ t()('receiptAllocated') }}:</dt><dd>{{ row.receipt_allocated_pieces }} EA</dd>
      <dt>{{ t()('receiptSequence') }}:</dt><dd>{{ row.through_receipt_sequence }}</dd>
    </dl>`,
  styles: [`dl { margin-block:.5rem; }`],
})
class ReceiptAllocationDetails {
  readonly allocation = input.required<ReceiptAllocation>();
  readonly t = inject(Preferences).t;
}

@Component({
  selector: 'erp-purchase-receipt',
  imports: [Feedback, ReceiptAllocationDetails, LucideCheck, LucideClipboardList, LucideRefreshCw, LucideSearch, LucideX],
  templateUrl: './purchase-receipt.html',
  styles: [`
    :host { display:block; }
    .receiving { margin-block:1.25rem; overflow-wrap:anywhere; }
    .receiving section { margin-block:1rem; }
    .receiving label { display:block; margin-block-start:.75rem; }
    .receiving select, .receiving input { max-width:100%; }
  `],
})
export class PurchaseReceiptPanel {
  readonly planId = input.required<string>();
  readonly orderId = input.required<string>();
  readonly lineId = input.required<string>();
  private readonly api = inject(ArticleApi);
  private readonly injector = inject(Injector);
  private readonly commands = inject(ReceiptCommands).session;
  readonly t = inject(Preferences).t;
  readonly lineKey = receiptLineKey;
  readonly validPieces = receiptPieces;
  private readonly confirmButton = viewChild<ElementRef<HTMLButtonElement>>('confirmButton');
  private readonly reviewButton = viewChild<ElementRef<HTMLButtonElement>>('reviewButton');

  private readonly access = computed<Access | null>(() => {
    const plan = this.planId(), order = this.orderId(), line = this.lineId();
    const grant = this.api.grant();
    this.api.scopedContext();
    if (!grant || !this.api.freshCredential() || !this.api.canReadReservation() || !this.api.canReadMaterialPlan()
      || ![plan, order, line, grant.tenant_id, grant.organization_id].every(receiptId)) return null;
    return {
      plan, key: `${grant.tenant_id}/${grant.organization_id}/${order}/${line}/${plan}`,
      sourcePath: `/v1/procurement/proposals/by-plan/${plan}/purchase-orders/receipt-source`,
      allocationBase: `/v1/inventory/reservations/${order}/${line}/material-plan/purchase-orders`,
      headers: this.api.requestHeaders(), control: new AbortController(),
    };
  });
  readonly view = linkedSignal({source: this.access, computation: empty});
  readonly pending = computed(() => {
    const access = this.access();
    return access && !this.view().denied ? this.commands()().get(access.key) ?? null : null;
  });
  readonly visible = computed(() => !!this.access()
    && (this.api.canDiscoverReceiptSource() || (!!this.pending() && (this.canRead() || this.canPost()))));
  readonly canRead = this.api.canReadReceiptAllocation;
  readonly canPost = this.api.canPostPurchaseReceipt;
  readonly canDiscover = this.api.canDiscoverReceiptSource;
  readonly selected = computed(() => this.pending()?.line ?? this.view().selected);

  constructor() {
    effect(onCleanup => {
      const access = this.access();
      if (access) onCleanup(() => access.control.abort());
    });
  }

  private active(access: Access) {
    return access === this.access() && !access.control.signal.aborted && this.api.credentialStillCurrent();
  }

  private request(access: Access, path: string, intent?: ReceiptIntent) {
    return fetch(path, {
      method: intent ? 'POST' : 'GET', body: intent ? JSON.stringify(intent) : undefined,
      headers: {...access.headers, ...(intent ? {'Content-Type': 'application/json'} : {})},
      signal: AbortSignal.any([access.control.signal, AbortSignal.timeout(intent ? 12000 : 6000)]),
      redirect: 'error', cache: 'no-store',
    });
  }

  private patch(values: Partial<View>) {
    this.view.update(view => ({...view, ...values}));
  }

  private setPending(access: Access, pending: PendingReceipt | null) {
    this.commands().update(old => {
      const next = new Map(old);
      if (pending) next.set(access.key, pending);
      else next.delete(access.key);
      return next;
    });
  }

  private denied(access: Access, response: Response) {
    if (response.status !== 401 && response.status !== 403) return false;
    access.control.abort();
    this.view.set({...empty(), denied: true});
    return true;
  }

  private linePath(access: Access, line: ReceiptSourceLine) {
    return `${access.allocationBase}/${line.purchase_order_id}/lines/${line.purchase_line_id}/receipts`;
  }

  async discover() {
    const access = this.access();
    if (!access || !this.active(access) || !this.canDiscover() || this.view().busy || this.pending()) return;
    this.view.set({...empty(), busy: 'source', feedback: 'busy'});
    try {
      const response = await this.request(access, access.sourcePath);
      if (!this.active(access) || this.denied(access, response)) return;
      if (response.status === 404) {
        this.patch({feedback: 'receiptSourceMissing'});
        return;
      }
      const lines = response.status === 200
        ? receiptSource(await readBoundedJson(response, MAX_RECEIPT_RESPONSE_BYTES), access.plan) : null;
      if (this.active(access)) this.patch({lines: lines ?? [], feedback: lines ? null : 'receiptSourceUnavailable'});
    } catch {
      if (this.active(access)) this.patch({feedback: 'receiptSourceUnavailable'});
    } finally {
      if (this.active(access)) this.patch({busy: null});
    }
  }

  select(key: string) {
    const access = this.access(), view = this.view();
    if (!access || !this.active(access) || view.busy || this.pending()) return;
    const selected = view.lines.find(line => receiptLineKey(line) === key) ?? null;
    this.view.set({...empty(), lines: view.lines, selected});
  }

  changePieces(pieces: string) {
    this.patch({draft: pieces});
  }

  review() {
    const access = this.access(), view = this.view();
    if (!access || !this.active(access) || view.busy || !view.selected || !this.canDiscover() || !this.canPost() || this.pending()
      || !receiptPieces(view.draft)) return;
    this.patch({review: view.draft, feedback: null});
    afterNextRender(() => this.confirmButton()?.nativeElement.focus(), {injector: this.injector});
  }

  cancelReview() {
    this.patch({review: null});
    afterNextRender(() => this.reviewButton()?.nativeElement.focus(), {injector: this.injector});
  }

  async read() {
    const access = this.access(), line = this.selected();
    if (!access || !this.active(access) || !line || !this.canRead() || this.view().busy) return;
    this.patch({busy: 'read', current: null, readFeedback: 'busy', review: null});
    try {
      const response = await this.request(access, this.linePath(access, line));
      if (!this.active(access) || this.denied(access, response)) return;
      if (response.status === 404) {
        this.patch({readFeedback: 'receiptNoneObserved'});
        return;
      }
      const current = response.status === 200
        ? receiptAllocation(await readBoundedJson(response, MAX_RECEIPT_RESPONSE_BYTES), access.plan, line) : null;
      if (this.active(access)) this.patch({current, readFeedback: current ? 'receiptReadSnapshot' : 'receiptReadUnavailable'});
    } catch {
      if (this.active(access)) this.patch({readFeedback: 'receiptReadUnavailable'});
    } finally {
      if (this.active(access)) this.patch({busy: null});
    }
  }

  confirm() {
    const access = this.access(), view = this.view();
    if (!access || !this.active(access) || view.busy || this.pending() || !view.selected || !this.canDiscover()
      || !this.canPost() || !receiptPieces(view.review)) return;
    const pending = {line: view.selected, intent: {receipt_id: crypto.randomUUID(), pieces: view.review}};
    this.setPending(access, pending);
    void this.post(access, pending, false);
  }

  retry() {
    const access = this.access(), pending = this.pending();
    if (access && this.active(access) && pending && !pending.needsOperator && this.canPost() && !this.view().busy) {
      void this.post(access, pending, true);
    }
  }

  private async post(access: Access, pending: PendingReceipt, retry: boolean) {
    const {line, intent} = pending;
    this.patch({busy: 'post', feedback: 'busy', review: null, current: null, readFeedback: 'receiptNotChecked'});
    try {
      const response = await this.request(access, this.linePath(access, line), intent);
      if (!this.active(access)) return;
      if (!retry && (response.status === 401 || response.status === 403)) this.setPending(access, null);
      if (this.denied(access, response)) return;
      if (response.status === 201 || response.status === 200) {
        const posted = postedReceipt(await readBoundedJson(response, MAX_RECEIPT_RESPONSE_BYTES), intent, access.plan, line);
        if (!this.active(access)) return;
        if (posted) {
          this.patch({selected: line, posted, feedback: 'receiptRecorded', draft: ''});
          this.setPending(access, null);
          return;
        }
      } else if (response.status === 409) {
        const feedback = receiptConflict(await readBoundedJson(response, MAX_RECEIPT_RESPONSE_BYTES));
        if (!this.active(access)) return;
        if (feedback === 'receiptOver' || feedback === 'receiptIneligible') this.setPending(access, null);
        if (feedback === 'receiptChanged') this.setPending(access, {...pending, needsOperator: true});
        this.patch({feedback: feedback === 'receiptChanged' ? null : feedback});
        return;
      } else if ([400, 404, 413].includes(response.status)) {
        if (!retry) this.setPending(access, null);
        this.patch({feedback: response.status === 404 ? 'receiptLineMissing' : 'receiptInvalidCommand'});
        return;
      }
      if (this.active(access)) this.patch({feedback: 'receiptUnknown'});
    } catch {
      if (this.active(access)) this.patch({feedback: 'receiptUnknown'});
    } finally {
      if (this.active(access)) this.patch({busy: null});
    }
  }
}
