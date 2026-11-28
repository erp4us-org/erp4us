import { NgTemplateOutlet } from '@angular/common';
import { Component, effect, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { RouterLink } from '@angular/router';
import { LucideCalculator } from '@lucide/angular';
import { ArticleApi } from './article-api';
import type { Message } from './messages';
import { Preferences } from './preferences';
import { Feedback, PageHeading } from './ui';

interface Product { sku: string; name: string; revision: string }
interface Node { sku: string; name: string; pieces: string; revision: string | null; children: Node[] }
interface Material { sku: string; name: string; pieces: string; unit: string }
interface Plan { tree: Node[]; materials: Material[]; sku: string; revision: string; pieces: string }

@Component({
  imports: [FormsModule, NgTemplateOutlet, RouterLink, LucideCalculator, Feedback, PageHeading],
  template: `
    <erp-page-heading [title]="t()('manufacturing')" [description]="t()('manufacturingIntro')" />
    @if (!api.canPlan()) {
      <p>{{ t()('noPlanningGrant') }}</p><a routerLink="/local-access">{{ t()('access') }}</a>
    } @else {
      @if (products().length) {
        <form class="card form-width" (ngSubmit)="calculate()">
          <label for="planning-product">{{ t()('planningProduct') }}</label>
          <select id="planning-product" name="product" [(ngModel)]="productIndex">
            @for (item of products(); track $index) { <option [ngValue]="$index">{{ item.sku }} · {{ item.name }} ({{ item.revision }})</option> }
          </select>
          <label for="planning-pieces">{{ t()('planningPieces') }}</label>
          <input id="planning-pieces" name="pieces" inputmode="numeric" autocomplete="off" [(ngModel)]="pieces" required>
          <div class="actions">
            <button type="submit" class="primary" [disabled]="pending()">
              <svg lucideCalculator aria-hidden="true"></svg>{{ t()('calculatePlan') }}
            </button>
          </div>
        </form>
      }
      <erp-feedback [message]="t()(message())" />
      @if (result(); as plan) {
        <section class="card">
          <h2>{{ t()('structure') }}: {{ plan.sku }} · {{ plan.revision }} × {{ plan.pieces }}</h2>
          <ng-template #tree let-nodes>
            <ul>
              @for (node of nodes; track $index) {
                <li>{{ node.pieces }} × {{ node.sku }} · {{ node.name }}
                  @if (node.revision) { ({{ node.revision }}) }
                  @if (node.children.length) { <ng-container *ngTemplateOutlet="tree; context: { $implicit: node.children }" /> }
                </li>
              }
            </ul>
          </ng-template>
          <ng-container *ngTemplateOutlet="tree; context: { $implicit: plan.tree }" />
        </section>
        <section class="card planning-table">
          <h2>{{ t()('grossMaterials') }}</h2>
          <table>
            <thead><tr><th scope="col">{{ t()('sku') }}</th><th scope="col">{{ t()('name') }}</th><th scope="col">{{ t()('planningPieces') }}</th></tr></thead>
            <tbody>
              @for (material of plan.materials; track material.sku) {
                <tr><th scope="row">{{ material.sku }}</th><td>{{ material.name }}</td><td>{{ material.pieces }} {{ material.unit }}</td></tr>
              }
            </tbody>
          </table>
        </section>
      }
    }
  `,
  styles: [`.planning-table { overflow: auto; } table { width: 100%; border-collapse: collapse; }
    th, td { padding: .35rem .6rem; text-align: left; }`],
})
export class Manufacturing {
  readonly t = inject(Preferences).t;
  readonly api = inject(ArticleApi);
  readonly products = signal<Product[]>([]);
  readonly pending = signal(false);
  readonly message = signal<Message>('busy');
  readonly result = signal<Plan | null>(null);
  productIndex = 0;
  pieces = '10';
  private calculation?: AbortController;

  constructor() {
    effect(onCleanup => {
      const allowed = this.api.canPlan();
      const headers = this.api.requestHeaders();
      this.calculation?.abort();
      this.pending.set(false);
      this.result.set(null);
      this.products.set([]);
      if (!allowed) return;
      const active = new AbortController();
      onCleanup(() => { active.abort(); this.calculation?.abort(); });
      this.message.set('busy');
      void this.load(headers, active.signal);
    });
  }

  private async load(headers: Record<string, string>, signal: AbortSignal) {
    try {
      const response = await fetch('/v1/manufacturing/products', {
        headers, signal: AbortSignal.any([signal, AbortSignal.timeout(5000)]), cache: 'no-store', redirect: 'error',
      });
      const products: Product[] = response.ok ? await response.json() : [];
      if (signal.aborted) return;
      this.productIndex = 0;
      this.products.set(products);
      this.message.set(products.length ? 'planningReady' : response.status === 404 ? 'planningEmpty' : 'unavailable');
    } catch { if (!signal.aborted) this.message.set('unavailable'); }
  }

  async calculate() {
    const product = this.products()[this.productIndex];
    this.result.set(null);
    if (!product || !/^(?:[1-9][0-9]{0,2}|1000)$/.test(this.pieces)) {
      this.message.set('planningInvalid');
      return;
    }
    this.calculation?.abort();
    const active = new AbortController();
    this.calculation = active;
    this.pending.set(true);
    this.message.set('busy');
    try {
      const response = await fetch('/v1/manufacturing/requirements:preview', {
        method: 'POST', headers: { ...this.api.requestHeaders(), 'Content-Type': 'application/json' },
        body: JSON.stringify({ sku: product.sku, revision: product.revision, pieces: this.pieces }),
        signal: AbortSignal.any([active.signal, AbortSignal.timeout(5000)]), cache: 'no-store', redirect: 'error',
      });
      const plan: Plan | null = response.ok ? await response.json() : null;
      if (active.signal.aborted) return;
      this.result.set(plan);
      this.message.set(plan ? 'planCalculated' : 'unavailable');
    } catch { if (!active.signal.aborted) this.message.set('unavailable'); }
    finally { if (!active.signal.aborted) this.pending.set(false); }
  }
}
