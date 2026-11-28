import { Component, effect, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { RouterLink } from '@angular/router';
import { LucideArrowRight, LucideCheck, LucideClipboardCheck, LucidePlus, LucideSave, LucideX } from '@lucide/angular';
import { ArticleApi } from './article-api';
import type { Message } from './messages';
import { Preferences } from './preferences';
import { Feedback, PageHeading } from './ui';

interface Customer { id: string; name: string; code: string; profile?: {currency: string} }
interface Product { sku: string; revision: string; name: string; price_eur: string }
interface Order {
  id: string; customer: Customer; status: 'draft' | 'confirmed'; revision: number; currency: string; total: string;
  confirmed_at?: string; confirmation_event_id?: string;
  line: {id: string; sku: string; product_revision: string; pieces: string; unit_price: string; unit: string};
}
interface Page<T> { items: T[]; next_after: string | null }

async function fetchJson<T>(path: string, headers: Record<string, string>, signal: AbortSignal, body?: unknown): Promise<T> {
  const response = await fetch(path, {
    headers: body === undefined ? headers : { ...headers, 'Content-Type': 'application/json' },
    method: body === undefined ? 'GET' : 'POST',
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.any([signal, AbortSignal.timeout(8000)]),
    cache: 'no-store', redirect: 'error',
  });
  if (!response.ok) throw response.status;
  return response.json();
}

@Component({
  imports: [FormsModule, RouterLink, LucideArrowRight, LucideCheck, LucideClipboardCheck, LucidePlus, LucideSave,
    LucideX, Feedback, PageHeading],
  template: `
    <erp-page-heading [title]="t()('salesOrders')" [description]="t()('salesIntro')" />
    @if (!api.canReadSales()) {
      <p>{{ t()('salesReadRequired') }}</p><a routerLink="/local-access">{{ t()('access') }}</a>
    } @else {
      @if (api.canCreateSalesDraft()) {
        <form class="card form-width" (ngSubmit)="create()">
          <h2>{{ t()('salesNewDraft') }}</h2>
          <label for="sales-customer">{{ t()('customer') }}</label>
          <select id="sales-customer" name="customer" [(ngModel)]="customerIndex" [disabled]="saving() || !!saved()">
            @for (item of customers(); track item.id; let i = $index) {
              <option [ngValue]="i">{{ item.code }} · {{ item.name }}</option>
            }
          </select>
          @if (nextCustomer(); as cursor) {
            <button type="button" (click)="moreCustomers(cursor)" [disabled]="saving() || !!saved()">
              <svg lucideArrowRight aria-hidden="true"></svg>{{ t()('salesNextCustomers') }}
            </button>
          }
          <label for="sales-product">{{ t()('planningProduct') }}</label>
          <select id="sales-product" name="product" [(ngModel)]="productIndex" [disabled]="saving() || !!saved()">
            @for (item of products(); track $index; let i = $index) {
              <option [ngValue]="i">{{ item.sku }} · {{ item.name }} ({{ item.revision }}) · EUR {{ item.price_eur }}</option>
            }
          </select>
          <label for="sales-pieces">{{ t()('planningPieces') }}</label>
          <input id="sales-pieces" name="pieces" inputmode="numeric" autocomplete="off" [(ngModel)]="pieces"
            [disabled]="saving() || !!saved()" required>
          <div class="actions">
            @if (saved(); as order) {
              <p>{{ t()('salesSaved') }}: {{ order.currency }} {{ order.total }}</p>
              <button type="button" (click)="newDraft()">
                <svg lucidePlus aria-hidden="true"></svg>{{ t()('salesNewDraft') }}
              </button>
            } @else {
              <button type="submit" class="primary" [disabled]="saving() || !customers().length || !products().length">
                <svg lucideSave aria-hidden="true"></svg>{{ t()('salesSaveDraft') }}
              </button>
            }
          </div>
          <erp-feedback [message]="formMessage() ? t()(formMessage()!) : ''" />
        </form>
      }
      <section class="card sales-list">
        <h2>{{ t()('salesSavedOrders') }}</h2>
        <erp-feedback [message]="listMessage() ? t()(listMessage()!) : ''" />
        @if (orders().items.length) {
          <table>
            <thead><tr><th scope="col">{{ t()('customer') }}</th><th scope="col">{{ t()('planningProduct') }}</th>
            <th scope="col">{{ t()('planningPieces') }}</th><th scope="col">{{ t()('salesOrderTotal') }}</th>
            <th scope="col">{{ t()('salesOrderStatus') }}</th>
            @if (api.canConfirmSales() || api.canReadReservation()) { <th scope="col">{{ t()('salesActions') }}</th> }</tr></thead>
            <tbody>
              @for (order of orders().items; track order.id) {
                <tr><th scope="row">{{ order.customer.name }} <small>{{ order.id }}</small></th>
                <td>{{ order.line.sku }} ({{ order.line.product_revision }})</td>
                <td>{{ order.line.pieces }} {{ order.line.unit }}</td><td>{{ order.currency }} {{ order.total }}</td>
                <td>{{ t()(order.status === 'confirmed' ? 'salesStatusConfirmed' : 'salesStatusDraft') }}</td>
                @if (api.canConfirmSales() || api.canReadReservation()) {
                  <td>
                    @if (order.status === 'draft' && api.canConfirmSales()) {
                      <button type="button" (click)="review(order)" [disabled]="confirmationPending()">
                        <svg lucideClipboardCheck aria-hidden="true"></svg>{{ t()('salesReviewConfirmation') }}
                      </button>
                    }
                    @if (order.status === 'confirmed' && api.canReadReservation()) {
                      <a [routerLink]="['/sales/orders', order.id, 'reservations', order.line.id]">{{ t()('reservationLink') }}</a>
                    }
                  </td>
                }</tr>
              }
            </tbody>
          </table>
        }
        @if (orders().next_after; as cursor) {
          <button type="button" (click)="moreOrders(cursor)">
            <svg lucideArrowRight aria-hidden="true"></svg>{{ t()('nextPage') }}
          </button>
        }
      </section>
      @if (api.canConfirmSales() && reviewOrder(); as order) {
        <section class="card form-width" aria-labelledby="sales-confirm-heading">
          <h2 id="sales-confirm-heading">{{ t()('salesReviewConfirmation') }}</h2>
          <p>{{ order.customer.name }} · {{ order.line.pieces }} {{ order.line.unit }} × {{ order.line.sku }}
            · {{ order.currency }} {{ order.total }}</p>
          <p>{{ t()('salesConfirmationHelp') }}</p>
          <erp-feedback [message]="confirmationMessage() ? t()(confirmationMessage()!) : ''" />
          <div class="actions">
            <button type="button" class="primary" (click)="confirm()" [disabled]="confirmationPending()">
              <svg lucideCheck aria-hidden="true"></svg>{{ t()('salesConfirm') }}
            </button>
            <button type="button" (click)="cancelReview()" [disabled]="confirmationPending()">
              <svg lucideX aria-hidden="true"></svg>{{ t()('salesCancelReview') }}
            </button>
          </div>
        </section>
      }
      <erp-feedback [message]="confirmationNotice() ? t()(confirmationNotice()!) : ''" />
    }
  `,
  styles: [`.sales-list { overflow:auto; } table { width:100%; border-collapse:collapse; }
    td, th { text-align:left; padding:.5rem; } td button { white-space: nowrap; }
    th small { display:block; font-weight:normal; overflow-wrap:anywhere; }`],
})
export class SalesOrders {
  readonly api = inject(ArticleApi);
  readonly t = inject(Preferences).t;
  readonly customers = signal<Customer[]>([]);
  readonly products = signal<Product[]>([]);
  readonly nextCustomer = signal<string | null>(null);
  readonly orders = signal<Page<Order>>({items: [], next_after: null});
  readonly formMessage = signal<Message | null>(null);
  readonly listMessage = signal<Message | null>(null);
  readonly saving = signal(false);
  readonly saved = signal<Order | null>(null);
  readonly reviewOrder = signal<Order | null>(null);
  readonly confirmationPending = signal(false);
  readonly confirmationMessage = signal<Message | null>(null);
  readonly confirmationNotice = signal<Message | null>(null);
  customerIndex = 0;
  productIndex = 0;
  pieces = '';
  private access?: {headers: Record<string, string>; signal: AbortSignal};
  private orderLoad?: AbortController;
  private customerLoad?: AbortController;
  private command?: {identity: string; id: string};
  private confirmationId?: string;

  constructor() {
    effect(onCleanup => {
      const canRead = this.api.canReadSales();
      const canCreate = this.api.canCreateSalesDraft();
      const headers = this.api.requestHeaders();
      this.access = undefined;
      this.customers.set([]);
      this.products.set([]);
      this.nextCustomer.set(null);
      this.orders.set({items: [], next_after: null});
      this.newDraft();
      this.cancelReview();
      this.confirmationPending.set(false);
      this.confirmationNotice.set(null);
      this.productIndex = 0;
      this.customerIndex = 0;
      this.saving.set(false);
      if (!canRead) return;
      const active = new AbortController();
      this.access = {headers, signal: active.signal};
      onCleanup(() => active.abort());
      void this.moreOrders();
      if (canCreate) {
        void this.moreCustomers();
        void this.loadProducts(this.access);
      }
    });
  }

  async moreOrders(cursor?: string) {
    const access = this.access;
    if (!access) return;
    this.orderLoad?.abort();
    this.orderLoad = new AbortController();
    const signal = AbortSignal.any([access.signal, this.orderLoad.signal]);
    this.listMessage.set('busy');
    try {
      const path = '/v1/sales/orders' + (cursor ? `?after=${encodeURIComponent(cursor)}` : '');
      const page = await fetchJson<Page<Order>>(path, access.headers, signal);
      if (!signal.aborted) {
        this.orders.set(page);
        this.listMessage.set(page.items.length ? null : 'salesNoOrders');
      }
    } catch { if (!signal.aborted) this.listMessage.set('unavailable'); }
  }

  async moreCustomers(cursor?: string) {
    const access = this.access;
    if (!access) return;
    this.customerLoad?.abort();
    this.customerLoad = new AbortController();
    const signal = AbortSignal.any([access.signal, this.customerLoad.signal]);
    try {
      const query = new URLSearchParams({role: 'customer', limit: '50'});
      if (cursor) query.set('after', cursor);
      const page = await fetchJson<Page<Customer>>(`/v1/partners?${query}`, access.headers, signal);
      if (!signal.aborted) {
        this.customerIndex = 0;
        this.customers.set(page.items.filter(customer => customer.profile?.currency === 'EUR'));
        this.nextCustomer.set(page.next_after);
      }
    } catch { if (!signal.aborted) this.formMessage.set('unavailable'); }
  }

  private async loadProducts({headers, signal}: NonNullable<SalesOrders['access']>) {
    try {
      const products = await fetchJson<Product[]>('/v1/manufacturing/products', headers, signal);
      if (!signal.aborted) this.products.set(products);
    } catch { if (!signal.aborted) this.formMessage.set('unavailable'); }
  }

  newDraft() {
    this.command = undefined;
    this.saved.set(null);
    this.formMessage.set(null);
    this.pieces = '';
  }

  async create() {
    const access = this.access;
    const customer = this.customers()[this.customerIndex];
    const product = this.products()[this.productIndex];
    if (!access || this.saving() || this.saved() || !this.api.canCreateSalesDraft()) return;
    if (!customer || !product || !/^(?:[1-9][0-9]{0,2}|1000)$/.test(this.pieces)) {
      this.formMessage.set('salesInvalid');
      return;
    }
    const input = {customer_id: customer.id, sku: product.sku, product_revision: product.revision, pieces: this.pieces};
    const identity = JSON.stringify(input);
    if (this.command?.identity !== identity) this.command = {identity, id: crypto.randomUUID()};
    this.saving.set(true);
    this.formMessage.set('busy');
    try {
      const draft = await fetchJson<Order>('/v1/sales/orders', access.headers, access.signal, { ...input, id: this.command.id });
      if (!access.signal.aborted) {
        this.saved.set(draft);
        this.formMessage.set(null);
        void this.moreOrders();
      }
    } catch (status) {
      if (!access.signal.aborted) this.formMessage.set(status === 422 ? 'salesInvalid' : 'unavailable');
    } finally { if (!access.signal.aborted) this.saving.set(false); }
  }

  review(order: Order) {
    if (this.confirmationPending() || order.status !== 'draft' || !this.api.canConfirmSales()) return;
    if (this.reviewOrder()?.id !== order.id) this.confirmationId = crypto.randomUUID();
    this.confirmationNotice.set(null);
    this.confirmationMessage.set(null);
    this.reviewOrder.set(order);
  }

  cancelReview() {
    this.confirmationId = undefined;
    this.reviewOrder.set(null);
    this.confirmationMessage.set(null);
  }

  async confirm() {
    const order = this.reviewOrder();
    const command_id = this.confirmationId;
    const access = this.access;
    if (!order || !command_id || !access || this.confirmationPending() || !this.api.canConfirmSales()) return;
    this.confirmationPending.set(true);
    this.confirmationMessage.set('busy');
    try {
      const confirmed = await fetchJson<Order>(`/v1/sales/orders/${encodeURIComponent(order.id)}/confirmation`,
        access.headers, access.signal, {command_id, expected_revision: order.revision});
      if (!access.signal.aborted) {
        // Cancel an older page request so it cannot overwrite the updated row.
        this.orderLoad?.abort();
        this.orders.update(page => ({...page, items: page.items.map(item => item.id === confirmed.id ? confirmed : item)}));
        this.listMessage.set(null);
        this.cancelReview();
        this.confirmationNotice.set('salesConfirmationSaved');
      }
    } catch (status) {
      if (!access.signal.aborted) {
        this.confirmationMessage.set(status === 409 ? 'salesConfirmationStale' : 'unavailable');
      }
    } finally { if (!access.signal.aborted) this.confirmationPending.set(false); }
  }
}
