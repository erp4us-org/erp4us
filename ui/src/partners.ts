import { Component, effect, inject, signal } from '@angular/core';
import { RouterLink } from '@angular/router';
import { LucideArrowRight } from '@lucide/angular';
import { ArticleApi } from './article-api';
import type { Message } from './messages';
import { Preferences } from './preferences';
import { Feedback, PageHeading } from './ui';

type Role = 'customer' | 'supplier';
interface Address { street: string; postal_code: string; city: string; country: string }
interface Partner {
  id: string; code: string; name: string; roles: Role[];
  profile?: { contact: string; address: Address; delivery_address: Address; currency: string; payment_days: number };
}
interface Page { items: Partner[]; next_after: string | null }

@Component({
  imports: [RouterLink, LucideArrowRight, Feedback, PageHeading],
  template: `
    <erp-page-heading [title]="t()('partners')" [description]="t()('partnersIntro')" />
    @if (!api.canReadPartners()) {
      <p>{{ t()('partnerGrantRequired') }}</p><a routerLink="/local-access">{{ t()('access') }}</a>
    } @else {
      <section class="card partner-list">
        <label for="partner-role">{{ t()('partnerRole') }}</label>
        <select id="partner-role" #roleSelect [value]="role()" (change)="chooseRole(roleSelect.value)">
          <option value="">{{ t()('allPartners') }}</option>
          <option value="customer">{{ t()('customer') }}</option>
          <option value="supplier">{{ t()('supplier') }}</option>
        </select>
        <erp-feedback [message]="message() ? t()(message()!) : ''" />
        @if (page().items.length) {
          <table>
            <thead><tr><th scope="col">{{ t()('partnerCode') }}</th><th scope="col">{{ t()('name') }}</th>
            <th scope="col">{{ t()('partnerRole') }}</th><th scope="col">{{ t()('partnerContact') }}</th></tr></thead>
            <tbody>
              @for (partner of page().items; track partner.id) {
                <tr>
                  <th scope="row">{{ partner.code }}</th>
                  <td>{{ partner.name }}</td>
                  <td>{{ roleLabels(partner.roles) }}</td>
                  <td>@if (partner.profile; as profile) {
                    <details><summary>{{ profile.contact }}</summary>
                      <p>{{ t()('partnerAddress') }}: {{ address(profile.address) }}</p>
                      <p>{{ t()('deliveryAddress') }}: {{ address(profile.delivery_address) }}</p>
                      <p>{{ t()('paymentDays') }}: {{ profile.payment_days }} · {{ profile.currency }}</p>
                    </details>
                  } @else { — }</td>
                </tr>
              }
            </tbody>
          </table>
        }
        @if (page().next_after; as next) {
          <button type="button" (click)="cursor.set(next)">
            <svg lucideArrowRight aria-hidden="true"></svg>{{ t()('nextPage') }}
          </button>
        }
      </section>
    }
  `,
  styles: [`.partner-list {overflow:auto} table {width:100%; border-collapse:collapse} td,th {text-align:left; padding:.5rem}`],
})
export class Partners {
  readonly api = inject(ArticleApi);
  readonly t = inject(Preferences).t;
  readonly role = signal<Role | ''>('');
  readonly cursor = signal<string | null>(null);
  readonly page = signal<Page>({ items: [], next_after: null });
  readonly message = signal<Message | null>(null);
  private previousScope = '';

  constructor() {
    effect(onCleanup => {
      const scoped = this.api.canReadPartners();
      const headers = this.api.requestHeaders();
      const scope = `${headers['X-Erp4us-Tenant']}/${headers['X-Erp4us-Organization']}`;
      const role = this.role();
      const cursor = this.cursor();
      this.page.set({items: [], next_after: null});
      if (scope !== this.previousScope) {
        this.previousScope = scope;
        if (cursor) { this.cursor.set(null); return; }
      }
      if (!scoped) return;
      const control = new AbortController();
      onCleanup(() => control.abort());
      this.message.set('busy');
      void this.load(headers, role, cursor, control.signal);
    });
  }

  chooseRole(value: string) {
    this.cursor.set(null);
    this.role.set(value === 'customer' || value === 'supplier' ? value : '');
  }

  address(value: Address) { return `${value.street}, ${value.postal_code} ${value.city}, ${value.country}`; }
  roleLabels(roles: Role[]) { return roles.map(role => this.t()(role)).join(', '); }

  private async load(headers: Record<string, string>, role: Role | '', cursor: string | null, signal: AbortSignal) {
    const parameters = new URLSearchParams();
    if (role) parameters.set('role', role);
    if (cursor) parameters.set('after', cursor);
    try {
      const response = await fetch(`/v1/partners?${parameters}`, {
        headers, signal: AbortSignal.any([signal, AbortSignal.timeout(5000)]), cache: 'no-store', redirect: 'error',
      });
      if (!response.ok) throw new Error('partner query rejected');
      const page: Page = await response.json();
      if (!signal.aborted) {
        this.page.set(page);
        this.message.set(page.items.length ? null : 'noPartners');
      }
    } catch {
      if (!signal.aborted) this.message.set('unavailable');
    }
  }
}
