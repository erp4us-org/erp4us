import { Component, Injectable, computed, effect, inject, input, linkedSignal, untracked } from '@angular/core';
import { LucideCheck, LucideClipboardList, LucideRefreshCw, LucideX } from '@lucide/angular';
import { ArticleApi, isArticleId } from './article-api';
import type { BudgetApproval } from './budget-approval';
import type { Message } from './messages';
import { Preferences } from './preferences';
import { Feedback } from './ui';

interface PurchaseLine {
  purchase_line_id: string; sku: string; revision: string | null; unit: 'EA';
  pieces: string; unit_price: string; line_total: string;
}
interface PurchaseOrder {
  purchase_order_id: string; supplier: { id: string; code: string; name: string };
  total: string; lines: PurchaseLine[];
}
interface Batch {
  batch_id: string; plan_id: string; proposal_id: string; order_id: string; line_id: string;
  terms_revision: number; policy_id: string; policy_revision: number;
  state: 'issued'; currency: 'EUR'; total: string; issued_at: string; purchase_orders: PurchaseOrder[];
}
interface Access {
  plan: string; order: string; line: string; path: string; key: string;
  headers: Record<string, string>; control: AbortController;
}
type State =
  | {kind: 'checking' | 'missing' | 'issuing' | 'unknown' | 'unavailable' | 'conflict' | 'denied'}
  | {kind: 'issued'; batch: Batch};

const exactAmount = /^(?:0|[1-9][0-9]{0,14})\.[0-9]{2}(?![\s\S])/;
const wholePieces = /^(?:[1-9][0-9]{0,5}|1000000)(?![\s\S])/;
const id = (value: unknown) => typeof value === 'string' && isArticleId(value)
  && value !== '00000000-0000-0000-0000-000000000000';
const amount = (value: unknown) => typeof value === 'string' && exactAmount.test(value);
const revision = (value: unknown) => typeof value === 'number'
  && Number.isInteger(value) && value >= 1 && value <= 2147483647;

// Keep the command on route reloads in the same verified session. No credentials
// or command state survive sign-out or a full browser reload; GET reopens a
// committed batch after sign-in, and Procurement enforces one batch per plan.
@Injectable({providedIn: 'root'})
class IssuanceCommands {
  private readonly api = inject(ArticleApi);
  readonly byPlan = computed(() => {
    this.api.credentialGeneration();
    return new Map<string, Map<string, string>>();
  });
}

@Component({
  selector: 'erp-purchase-issuance',
  imports: [Feedback, LucideCheck, LucideClipboardList, LucideRefreshCw, LucideX],
  template: `
    <section class="purchase-issuance" [attr.aria-labelledby]="'purchase-issuance-' + planId()">
      <h3 [id]="'purchase-issuance-' + planId()">{{ t()(batch() ? 'purchaseIssued' : 'purchaseIssuance') }}</h3>
      <p>{{ t()('purchaseLocalOnly') }}</p>
      <erp-feedback [message]="message() ? t()(message()!) : ''" />

      @if (batch(); as issued) {
        <p>{{ t()('purchaseBatchId') }}: {{ issued.batch_id }} ·
          {{ t()('purchaseIssuedAt') }}: <time [attr.datetime]="issued.issued_at">{{ issued.issued_at }}</time></p>
        <p>{{ t()('purchaseIssuedTotal') }}: {{ issued.currency }} {{ issued.total }} ·
          {{ t()('purchaseTermsVersion') }}: {{ issued.terms_revision }} ·
          {{ t()('purchasePolicyVersion') }}: {{ issued.policy_id }} / {{ issued.policy_revision }}</p>
        @for (order of issued.purchase_orders; track order.purchase_order_id) {
          <h4>{{ order.supplier.code }} · {{ order.supplier.name }}</h4>
          <p>{{ t()('purchaseOrderId') }}: {{ order.purchase_order_id }} ·
            {{ t()('purchaseOrderTotal') }}: {{ issued.currency }} {{ order.total }}</p>
          <table [attr.aria-label]="order.supplier.code + ' · ' + order.supplier.name">
            <thead><tr><th scope="col">{{ t()('sku') }}</th><th scope="col">{{ t()('purchaseOrderedQuantity') }}</th>
              <th scope="col">{{ t()('purchaseTermsUnitPrice') }}</th><th scope="col">{{ t()('purchaseTermsLineTotal') }}</th></tr></thead>
            <tbody>
              @for (line of order.lines; track line.purchase_line_id) {
                <tr><th scope="row">{{ line.sku }} @if (line.revision) { · {{ line.revision }} }
                  <small>{{ t()('purchaseLineId') }}: {{ line.purchase_line_id }}</small></th>
                  <td>{{ line.pieces }} {{ line.unit }}</td><td>{{ issued.currency }} {{ line.unit_price }}</td>
                  <td>{{ issued.currency }} {{ line.line_total }}</td></tr>
              }
            </tbody>
          </table>
        }
      } @else {
        @if (candidate(); as approval) {
          @if (reviewing()) {
            <section [attr.aria-labelledby]="'purchase-review-' + planId()">
              <h4 [id]="'purchase-review-' + planId()">{{ t()('purchaseReviewTitle') }}</h4>
              <p>{{ t()('budgetApprovalBasis') }}: {{ approval.currency }} {{ approval.total }} ·
                {{ t()('purchaseTermsVersion') }}: {{ approval.terms_revision }} ·
                {{ t()('purchasePolicyVersion') }}: {{ approval.policy_id }} / {{ approval.policy_revision }}</p>
              <p>{{ t()('purchaseConfirmHelp') }}</p>
              <div class="actions">
                <button type="button" class="primary" (click)="confirm()">
                  <svg lucideCheck aria-hidden="true"></svg>{{ t()(isRetry() ? 'purchaseRetry' : 'purchaseConfirm') }}
                </button>
                <button type="button" (click)="reviewing.set(false)">
                  <svg lucideX aria-hidden="true"></svg>{{ t()('purchaseCancelReview') }}
                </button>
              </div>
            </section>
          } @else {
            <button type="button" (click)="reviewing.set(true)">
              <svg lucideClipboardList aria-hidden="true"></svg>{{ t()(isRetry() ? 'purchaseReviewRetry' : 'purchaseReview') }}
            </button>
          }
        }
        <button type="button" (click)="refresh()" [disabled]="busy() || !api.freshCredential()">
          <svg lucideRefreshCw aria-hidden="true"></svg>{{ t()('purchaseCheckBatch') }}
        </button>
      }
    </section>`,
  styles: [`.purchase-issuance table { width:100%; border-collapse:collapse; }
    .purchase-issuance th, .purchase-issuance td { text-align:left; padding:.4rem; }
    .purchase-issuance small { display:block; font-weight:normal; overflow-wrap:anywhere; }`],
})
export class PurchaseIssuancePanel {
  readonly planId = input.required<string>();
  readonly orderId = input.required<string>();
  readonly lineId = input.required<string>();
  readonly approval = input<BudgetApproval | null>(null);
  readonly api = inject(ArticleApi);
  readonly t = inject(Preferences).t;
  private readonly commands = inject(IssuanceCommands).byPlan;

  private readonly access = computed<Access | null>(() => {
    const plan = this.planId(), order = this.orderId(), line = this.lineId();
    const grant = this.api.grant();
    this.api.scopedContext();
    if (!grant || !this.api.canReadPurchaseOrders() || !this.api.freshCredential() || ![plan, order, line].every(id)) return null;
    return {
      plan, order, line, path: `/v1/procurement/proposals/by-plan/${plan}/purchase-orders`,
      key: `${grant.tenant_id}/${grant.organization_id}/${plan}`,
      headers: this.api.requestHeaders(), control: new AbortController(),
    };
  });
  private readonly state = linkedSignal<Access | null, State>({source: this.access, computation: () => ({kind: 'checking'})});
  private readonly issueDenied = linkedSignal({source: this.access, computation: () => false});
  private readonly conflictBasis = linkedSignal<Access | null, BudgetApproval | null>({source: this.access, computation: () => null});
  readonly batch = computed(() => {
    const state = this.state();
    return state.kind === 'issued' ? state.batch : null;
  });
  readonly busy = computed(() => ['checking', 'issuing'].includes(this.state().kind));
  readonly mayEditDraft = computed(() => this.state().kind === 'missing');
  readonly candidate = computed(() => {
    const access = this.access(), approval = this.approval();
    return access && approval?.plan_id === access.plan && approval.state === 'approved'
      && id(approval.proposal_id) && id(approval.policy_id)
      && revision(approval.terms_revision) && revision(approval.policy_revision) && amount(approval.total)
      && this.api.canIssuePurchaseOrders() && !this.issueDenied() && this.state().kind === 'missing'
      && this.conflictBasis() !== approval ? approval : null;
  });
  readonly reviewing = linkedSignal({source: this.candidate, computation: () => false});
  readonly isRetry = computed(() => {
    const access = this.access(), approval = this.candidate();
    return !!access && !!approval && this.commands().get(access.key)?.has(this.intentKey(approval)) === true;
  });
  readonly message = computed<Message | null>(() => {
    if (!this.api.freshCredential()) return 'purchaseDenied';
    switch (this.state().kind) {
      case 'checking': case 'issuing': return 'busy';
      case 'denied': return 'purchaseDenied';
      case 'unavailable': return 'purchaseLookupUnavailable';
      case 'unknown': return 'purchaseOutcomeUnknown';
      case 'conflict': return 'purchaseIssueConflict';
      case 'missing':
        if (this.issueDenied()) return 'purchaseDenied';
        if (this.conflictBasis() === this.approval() && this.conflictBasis()) return 'purchaseIssueConflict';
        return this.isRetry() ? 'purchaseRetryHelp' : null;
      case 'issued': return null;
    }
  });

  constructor() {
    effect(onCleanup => {
      const access = this.access();
      if (!access) return;
      onCleanup(() => access.control.abort());
      untracked(() => void this.check(access));
    });
  }

  refresh() {
    const access = this.access();
    if (access && !this.busy() && !this.batch()) void this.check(access);
  }

  confirm() {
    const access = this.access(), approval = this.candidate();
    if (!access || !approval || !this.reviewing() || !this.api.credentialStillCurrent()) return;
    const map = this.commands();
    const prior = map.get(access.key) ?? new Map<string, string>();
    const key = this.intentKey(approval);
    const command_id = prior.get(key) ?? crypto.randomUUID();
    prior.set(key, command_id);
    map.set(access.key, prior);
    void this.issue(access, approval, command_id);
  }

  private intentKey(approval: BudgetApproval) {
    return `${approval.proposal_id}/${approval.terms_revision}/${approval.policy_id}/${approval.policy_revision}`;
  }

  private isCurrent(access: Access) {
    return !access.control.signal.aborted && access === this.access() && this.api.credentialStillCurrent();
  }

  private request(access: Access, body?: string) {
    return fetch(access.path, {
      method: body === undefined ? 'GET' : 'POST', body,
      headers: {...access.headers, ...(body === undefined ? {} : {'Content-Type': 'application/json'})},
      signal: AbortSignal.any([access.control.signal, AbortSignal.timeout(body === undefined ? 6000 : 10000)]),
      cache: 'no-store', redirect: 'error',
    });
  }

  private async check(access: Access) {
    if (!this.isCurrent(access)) return;
    this.reviewing.set(false);
    this.state.set({kind: 'checking'});
    try {
      const response = await this.request(access);
      if (!this.isCurrent(access)) return;
      if (response.status === 404) this.state.set({kind: 'missing'});
      else if (response.status === 401 || response.status === 403) {
        this.issueDenied.set(true);
        this.state.set({kind: 'denied'});
      } else if (response.status === 200) await this.open(access, response);
      else this.state.set({kind: 'unavailable'});
    } catch {
      if (this.isCurrent(access)) this.state.set({kind: 'unavailable'});
    }
  }

  private async issue(access: Access, approval: BudgetApproval, command_id: string) {
    this.reviewing.set(false);
    this.state.set({kind: 'issuing'});
    const {terms_revision, policy_id, policy_revision} = approval;
    try {
      const response = await this.request(access, JSON.stringify({command_id, terms_revision, policy_id, policy_revision}));
      if (!this.isCurrent(access)) return;
      if (response.status === 200 || response.status === 201) await this.open(access, response, approval);
      else if (response.status === 401 || response.status === 403) {
        this.issueDenied.set(true);
        this.state.set({kind: 'denied'});
      } else if ([400, 404, 409].includes(response.status)) {
        this.conflictBasis.set(approval);
        this.state.set({kind: 'conflict'});
      } else this.state.set({kind: 'unknown'});
    } catch {
      if (this.isCurrent(access)) this.state.set({kind: 'unknown'});
    }
  }

  private async open(access: Access, response: Response, approval?: BudgetApproval) {
    const batch: Batch = await response.json();
    if (!this.isCurrent(access)) return;
    if (!batch || batch.plan_id !== access.plan || batch.order_id !== access.order || batch.line_id !== access.line
      || batch.state !== 'issued' || batch.currency !== 'EUR' || !id(batch.batch_id) || !id(batch.proposal_id)
      || !id(batch.policy_id) || !revision(batch.policy_revision) || !revision(batch.terms_revision)
      || !amount(batch.total) || typeof batch.issued_at !== 'string' || !Number.isFinite(Date.parse(batch.issued_at))
      || !Array.isArray(batch.purchase_orders) || !batch.purchase_orders.length
      || batch.purchase_orders.some(order => !order || !id(order.purchase_order_id) || !id(order.supplier?.id)
        || typeof order.supplier.code !== 'string' || typeof order.supplier.name !== 'string' || !amount(order.total)
        || !Array.isArray(order.lines) || !order.lines.length || order.lines.some(line => !line
          || !id(line.purchase_line_id) || typeof line.sku !== 'string'
          || (line.revision !== null && typeof line.revision !== 'string') || line.unit !== 'EA'
          || typeof line.pieces !== 'string' || !wholePieces.test(line.pieces)
          || !amount(line.unit_price) || !amount(line.line_total)))
      || (approval && (batch.proposal_id !== approval.proposal_id || batch.terms_revision !== approval.terms_revision
        || batch.policy_id !== approval.policy_id || batch.policy_revision !== approval.policy_revision || batch.total !== approval.total))) {
      throw new Error('Invalid issued purchase batch');
    }
    this.commands().delete(access.key);
    this.state.set({kind: 'issued', batch});
  }
}
