import { Component, computed, effect, inject, signal } from '@angular/core';
import { toSignal } from '@angular/core/rxjs-interop';
import { ActivatedRoute, RouterLink } from '@angular/router';
import { LucideListChecks, LucideRefreshCw } from '@lucide/angular';
import { ArticleApi, isArticleId } from './article-api';
import type { Message } from './messages';
import { Preferences } from './preferences';
import { ProcurementHandoffPanel } from './procurement-handoff';
import { PurchaseReceiptPanel } from './purchase-receipt';
import { Feedback, PageHeading } from './ui';

interface Reservation {
  order_id: string; line_id: string; warehouse_id: string; sku: string; product_revision: string;
  unit: string; requested: string; reserved: string; outstanding: string; source_event_id: string;
}
interface PlanRow { sku: string; revision: string | null; requested: string; reserved: string; uncovered: string }
interface MaterialPlan { id: string; order_id: string; line_id: string; production: PlanRow[]; materials: PlanRow[] }
interface Access {
  order: string; line: string; path: string; headers: Record<string, string>; signal: AbortSignal;
  context: ReturnType<ArticleApi['scopedContext']>;
}

@Component({
  imports: [RouterLink, LucideListChecks, LucideRefreshCw, Feedback, PageHeading, ProcurementHandoffPanel, PurchaseReceiptPanel],
  template: `
    <erp-page-heading [title]="t()('reservationLink')" [description]="t()('reservationIntro')" />
    <a routerLink="/sales/orders">{{ t()('salesOrders') }}</a>
    @if (!api.canReadReservation()) {
      <p>{{ t()('reservationDenied') }}</p>
    } @else {
      <section class="card">
        <erp-feedback [message]="message() ? t()(message()!) : ''" />
        @if (reservation(); as row) {
          <h2>{{ row.sku }} · {{ row.product_revision }}</h2>
          <dl>
            <dt>{{ t()('reservationRequested') }}</dt><dd>{{ row.requested }} {{ row.unit }}</dd>
            <dt>{{ t()('reservationReserved') }}</dt><dd>{{ row.reserved }} {{ row.unit }}</dd>
            <dt>{{ t()('reservationOutstanding') }}</dt><dd>{{ row.outstanding }} {{ row.unit }}</dd>
            <dt>{{ t()('reservationWarehouse') }}</dt><dd>{{ row.warehouse_id }}</dd>
            <dt>{{ t()('reservationEvent') }}</dt><dd>{{ row.source_event_id }}</dd>
          </dl>
        }
        <button type="button" (click)="refresh.update(n => n + 1)" [disabled]="message() === 'busy'">
          <svg lucideRefreshCw aria-hidden="true"></svg>{{ t()('reservationRefresh') }}
        </button>
      </section>
      @if (reservation() && api.canReadMaterialPlan()) {
        <section class="card material-plan">
          <h2>{{ t()('materialPlan') }}</h2>
          <p>{{ t()('materialPlanIntro') }}</p>
          <erp-feedback [message]="planMessage() ? t()(planMessage()!) : ''" />
          @if (plan(); as stored) {
            @for (group of planGroups(stored); track group.label) {
              <table [attr.aria-label]="t()(group.label)">
                <caption>{{ t()(group.label) }}</caption>
                <thead><tr><th scope="col">{{ t()('sku') }}</th><th scope="col">{{ t()('reservationRequested') }}</th>
                  <th scope="col">{{ t()('reservationReserved') }}</th><th scope="col">{{ t()(group.uncovered) }}</th></tr></thead>
                <tbody>
                  @for (row of group.rows; track row.sku + '/' + (row.revision ?? '')) {
                    <tr><th scope="row">{{ row.sku }} @if (row.revision) { · {{ row.revision }} }</th>
                      <td>{{ row.requested }} EA</td><td>{{ row.reserved }} EA</td><td>{{ row.uncovered }} EA</td></tr>
                  }
                </tbody>
              </table>
            }
            @if (reservation(); as row) {
              <erp-procurement-handoff
                [orderId]="row.order_id" [lineId]="row.line_id" [planId]="stored.id"
                [hasShortages]="hasShortages(stored)" />
            }
            @if (receivingPlan(); as context) {
              <erp-purchase-receipt [orderId]="context.order" [lineId]="context.line" [planId]="context.plan" />
            }
          } @else if (api.canCreateMaterialPlan() && reservation()?.outstanding !== '0') {
            <button type="button" class="primary" (click)="planMaterials()" [disabled]="planBusy()">
              <svg lucideListChecks aria-hidden="true"></svg>{{ t()('materialPlanCreate') }}
            </button>
          }
        </section>
      }
    }
  `,
  styles: [`.material-plan { overflow:auto; }
    .material-plan table { width:100%; border-collapse:collapse; margin-bottom:1rem; }
    .material-plan th, .material-plan td, .material-plan caption { text-align:left; padding:.4rem; }`],
})
export class OrderReservation {
  readonly api = inject(ArticleApi);
  readonly t = inject(Preferences).t;
  private readonly params = toSignal(inject(ActivatedRoute).paramMap);
  readonly refresh = signal(0);
  readonly message = signal<Message | null>(null);
  readonly reservation = signal<Reservation | null>(null);
  readonly plan = signal<MaterialPlan | null>(null);
  readonly planMessage = signal<Message | null>(null);
  readonly planBusy = signal(false);
  private readonly receiptBasis = signal<Access | null>(null);
  readonly receivingPlan = computed(() => {
    const access = this.receiptBasis(), plan = this.plan(), row = this.reservation(), params = this.params();
    if (!access || access.signal.aborted || access.context !== this.api.scopedContext() || !this.api.freshCredential()
      || params?.get('orderId') !== access.order || params?.get('lineId') !== access.line
      || plan?.order_id !== access.order || plan.line_id !== access.line
      || row?.order_id !== access.order || row.line_id !== access.line) return null;
    return {order: access.order, line: access.line, plan: plan.id};
  });
  private access?: Access;

  constructor() {
    effect(onCleanup => {
      this.refresh();
      const params = this.params();
      const context = this.api.scopedContext();
      const allowed = this.api.canReadReservation();
      const canReadPlan = this.api.canReadMaterialPlan();
      const headers = this.api.requestHeaders();
      this.access = undefined;
      this.reservation.set(null);
      this.message.set(null);
      this.plan.set(null);
      this.receiptBasis.set(null);
      this.planMessage.set(null);
      this.planBusy.set(false);
      if (!allowed) return;
      const order = params?.get('orderId');
      const line = params?.get('lineId');
      if (!order || !line || !isArticleId(order) || !isArticleId(line)) {
        this.message.set('reservationMissing');
        return;
      }
      const control = new AbortController();
      onCleanup(() => control.abort());
      this.access = { order, line, path: `/v1/inventory/reservations/${order}/${line}`, headers, signal: control.signal, context };
      this.message.set('busy');
      void this.load(this.access, canReadPlan);
    });
  }

  private async load(access: Access, canReadPlan: boolean) {
    const { path, headers, signal } = access;
    try {
      for (let attempt = 0; attempt < 5 && !signal.aborted; attempt++) {
        if (attempt) await new Promise<void>(resolve => setTimeout(resolve, 750));
        if (signal.aborted) return;
        const response = await fetch(path, {
          headers, signal: AbortSignal.any([signal, AbortSignal.timeout(5000)]),
          cache: 'no-store', redirect: 'error',
        });
        if (response.status === 404) continue;
        const row: Reservation | null = response.ok ? await response.json() : null;
        if (!signal.aborted) {
          this.reservation.set(row);
          this.message.set(row ? null : response.status === 403 ? 'reservationDenied' : 'unavailable');
          if (row && canReadPlan) void this.readPlan(access, 'GET');
        }
        return;
      }
      if (!signal.aborted) this.message.set('reservationMissing');
    } catch {
      if (!signal.aborted) this.message.set('unavailable');
    }
  }

  planGroups(plan: MaterialPlan): {label: Message; uncovered: Message; rows: PlanRow[]}[] {
    return [
      {label: 'materialProduction', uncovered: 'materialToMake', rows: plan.production},
      {label: 'materialPurchasing', uncovered: 'materialToBuy', rows: plan.materials},
    ];
  }

  hasShortages(plan: MaterialPlan) {
    return plan.materials.some(row => row.uncovered !== '0');
  }

  planMaterials() {
    if (this.access && this.api.canCreateMaterialPlan() && !this.planBusy()) {
      void this.readPlan(this.access, 'POST');
    }
  }

  private async readPlan(access: Access, method: 'GET' | 'POST') {
    const {path, headers, signal} = access;
    this.planBusy.set(true);
    this.planMessage.set('busy');
    try {
      const response = await fetch(`${path}/material-plan`, {
        method, headers, signal: AbortSignal.any([signal, AbortSignal.timeout(12000)]),
        cache: 'no-store', redirect: 'error',
      });
      const plan: MaterialPlan | null = response.ok ? await response.json() : null;
      if (!signal.aborted) {
        this.plan.set(plan);
        this.receiptBasis.set(plan ? access : null);
        this.planMessage.set(plan || (method === 'GET' && response.status === 404) ? null
          : response.status === 409 ? 'materialPlanConflict' : 'unavailable');
      }
    } catch {
      if (!signal.aborted) this.planMessage.set('unavailable');
    } finally {
      if (!signal.aborted) this.planBusy.set(false);
    }
  }
}
