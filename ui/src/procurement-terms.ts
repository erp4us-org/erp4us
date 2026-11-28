import { Component, computed, effect, inject, input, signal, viewChild } from '@angular/core';
import { LucideArrowRight, LucideRefreshCw, LucideSave } from '@lucide/angular';
import { ArticleApi, isArticleId } from './article-api';
import { BudgetApprovalPanel } from './budget-approval';
import type { Message } from './messages';
import { Preferences } from './preferences';
import { Feedback } from './ui';

interface Shortage { sku: string; revision: string | null; unit: string; pieces: string }
export interface PurchaseDraft {
  proposal_id: string; plan_id: string; order_id: string; line_id: string; status: 'draft'; lines: Shortage[];
}
interface Supplier { id: string; code: string; name: string }
interface Edit { supplier_id: string; unit_price: string }
interface Terms {
  proposal_id: string; plan_id: string; status: 'draft'; revision: number; currency: 'EUR'; total: string;
  lines: (Shortage & {supplier: Supplier; unit_price: string; line_total: string})[];
}
interface Access {
  draft: PurchaseDraft; headers: Record<string, string>; signal: AbortSignal; canEdit: boolean;
  context: ReturnType<ArticleApi['scopedContext']>; refresh: number;
}
const empty = { supplier_id: '', unit_price: '' };
const cents = /^(?:0|[1-9][0-9]{0,6})\.[0-9]{2}(?![\s\S])/;

@Component({
  selector: 'erp-procurement-terms',
  imports: [Feedback, LucideArrowRight, LucideRefreshCw, LucideSave, BudgetApprovalPanel],
  template: `
    <section class="terms">
      <h4>{{ t()('purchaseTerms') }}</h4>
      <p>{{ t()('purchaseTermsIntro') }}</p>
      <erp-feedback [message]="message() ? t()(message()!) : ''" />
      @if (stored(); as terms) {
        <p>{{ t()('purchaseTermsRevision') }}: {{ terms.revision }} ·
          {{ t()('purchaseTermsTotal') }}: {{ terms.currency }} {{ terms.total }}</p>
        <table [attr.aria-label]="t()('savedPurchaseTerms')">
          <thead><tr><th scope="col">{{ t()('sku') }}</th><th scope="col">{{ t()('supplier') }}</th>
            <th scope="col">{{ t()('purchaseTermsUnitPrice') }}</th><th scope="col">{{ t()('purchaseTermsLineTotal') }}</th></tr></thead>
          <tbody>
            @for (line of terms.lines; track label(line)) {
              <tr><th scope="row">{{ label(line) }}</th><td>{{ line.supplier.code }} · {{ line.supplier.name }}</td>
                <td>EUR {{ line.unit_price }}</td><td>EUR {{ line.line_total }}</td></tr>
            }
          </tbody>
        </table>
        <erp-budget-approval
          [planId]="terms.plan_id" [proposalId]="terms.proposal_id" [termsRevision]="terms.revision"
          [locked]="locked() || busy()" />
      }
      @if (api.canEditProposalTerms() && loaded() && !locked()) {
        <form (submit)="save($event)">
          <fieldset [disabled]="busy()">
            <legend>{{ t()('editPurchaseTerms') }}</legend>
            <table>
              <thead><tr><th scope="col">{{ t()('sku') }}</th><th scope="col">{{ t()('materialToBuy') }}</th>
                <th scope="col">{{ t()('supplier') }}</th><th scope="col">{{ t()('purchaseTermsUnitPrice') }}</th></tr></thead>
              <tbody>
                @for (line of draft().lines; track label(line)) {
                  <tr>
                    <th scope="row">{{ label(line) }}</th><td>{{ line.pieces }} {{ line.unit }}</td>
                    <td>
                      <select required #supplier [value]="edit(line).supplier_id"
                        [attr.aria-label]="t()('supplier') + ' ' + label(line)"
                        (change)="change(line, {supplier_id:supplier.value})">
                        <option value="">{{ t()('selectPurchaseSupplier') }}</option>
                        @for (option of suppliers(); track option.id) {
                          <option [value]="option.id" [selected]="option.id === edit(line).supplier_id">
                            {{ option.code }} · {{ option.name }}
                          </option>
                        }
                      </select>
                    </td>
                    <td>
                      <input type="text" inputmode="decimal" maxlength="10" required placeholder="0.00"
                        #price [attr.aria-label]="t()('purchaseTermsUnitPrice') + ' ' + label(line)"
                        [value]="edit(line).unit_price" (input)="change(line, {unit_price:price.value})">
                    </td>
                  </tr>
                }
              </tbody>
            </table>
            @if (next(); as cursor) {
              <button type="button" (click)="more(cursor)" [disabled]="loadingSuppliers()">
                <svg lucideArrowRight aria-hidden="true"></svg>{{ t()('morePurchaseSuppliers') }}
              </button>
            }
            <button type="submit" [disabled]="!complete() || !dirty() || conflict()">
              <svg lucideSave aria-hidden="true"></svg>{{ t()('savePurchaseTerms') }}
            </button>
          </fieldset>
        </form>
      }
      @if (conflict() || message() === 'unavailable') {
        <button type="button" (click)="reload.update(n => n + 1)" [disabled]="busy()">
          <svg lucideRefreshCw aria-hidden="true"></svg>{{ t()('reloadPurchaseTerms') }}
        </button>
      }
    </section>`,
  styles: [`.terms table { width:100%; border-collapse:collapse; }
    .terms td, .terms th { text-align:left; padding:.4rem; } .terms input { width:9em; }`],
})
export class ProcurementTermsPanel {
  readonly draft = input.required<PurchaseDraft>();
  readonly locked = input(false);
  readonly api = inject(ArticleApi);
  readonly t = inject(Preferences).t;
  readonly stored = signal<Terms | null>(null);
  readonly loaded = signal(false);
  readonly edits = signal<Record<string, Edit>>({});
  readonly suppliers = signal<Supplier[]>([]);
  readonly next = signal<string | null>(null);
  readonly message = signal<Message | null>(null);
  readonly busy = signal(false);
  readonly loadingSuppliers = signal(false);
  readonly conflict = signal(false);
  readonly reload = signal(0);
  private readonly budget = viewChild(BudgetApprovalPanel);
  readonly approved = computed(() => {
    const saved = this.stored(), approval = this.budget()?.approved();
    return saved && approval && this.isCurrent(this.access) && !this.busy() && !this.conflict()
      && approval.plan_id === saved.plan_id && approval.proposal_id === saved.proposal_id
      && approval.terms_revision === saved.revision && approval.currency === saved.currency
      && approval.total === saved.total ? approval : null;
  });
  readonly complete = computed(() => this.draft().lines.every(line => {
    const edit = this.edit(line);
    return cents.test(edit.unit_price) && this.suppliers().some(supplier => supplier.id === edit.supplier_id);
  }));
  readonly dirty = computed(() => {
    const saved = this.stored();
    return !saved || saved.lines.some(line => {
      const edit = this.edit(line);
      return edit.supplier_id !== line.supplier.id || edit.unit_price !== line.unit_price;
    });
  });
  private access?: Access;
  private pending?: { signature: string; id: string };

  constructor() {
    effect(onCleanup => {
      const refresh = this.reload(), context = this.api.scopedContext();
      const draft = this.draft();
      const headers = this.api.requestHeaders();
      const canEdit = this.api.canEditProposalTerms();
      this.access = undefined;
      this.pending = undefined;
      this.stored.set(null);
      this.loaded.set(false);
      this.edits.set({});
      this.suppliers.set([]);
      this.next.set(null);
      this.message.set(null);
      this.busy.set(false);
      this.loadingSuppliers.set(false);
      this.conflict.set(false);
      if (!this.api.canReadProposal() || !isArticleId(draft.plan_id)) return;
      const controller = new AbortController();
      onCleanup(() => controller.abort());
      this.access = {draft, headers, signal: controller.signal, canEdit, context, refresh};
      void this.load(this.access);
    });
  }

  label(line: Shortage) { return line.revision ? `${line.sku} · ${line.revision}` : line.sku; }
  edit(line: Shortage) { return this.edits()[this.label(line)] ?? empty; }
  change(line: Shortage, update: Partial<Edit>) {
    this.edits.update(edits => ({...edits, [this.label(line)]: {...this.edit(line), ...update}}));
  }

  more(cursor: string) {
    if (this.isCurrent(this.access) && !this.locked() && !this.loadingSuppliers() && cursor === this.next()) {
      void this.loadSuppliers(this.access, cursor);
    }
  }

  async save(event: Event) {
    event.preventDefault();
    const access = this.access;
    if (!this.isCurrent(access) || !access.canEdit || this.locked() || !this.loaded() || this.busy() || !this.complete()
      || !this.dirty() || this.conflict()) return;
    const { draft } = access;
    const intent = {
      expected_revision: this.stored()?.revision ?? 0,
      lines: draft.lines.map(line => ({sku:line.sku, revision:line.revision, ...this.edit(line)})),
    };
    const signature = JSON.stringify(intent);
    if (this.pending?.signature !== signature) this.pending = {signature, id:crypto.randomUUID()};
    this.busy.set(true);
    this.message.set('busy');
    try {
      const response = await this.request(access, this.path(draft.plan_id), JSON.stringify({
        command_id:this.pending.id, ...intent,
      }));
      if (!this.isCurrent(access)) return;
      if (response.status === 409) {
        this.conflict.set(true);
        this.message.set('purchaseTermsConflict');
      } else if (response.status === 400) {
        this.message.set('purchaseTermsInvalid');
      } else {
        const terms = await this.readTerms(response, draft);
        if (!this.isCurrent(access)) return;
        this.stored.set(terms);
        this.pending = undefined;
        this.message.set('purchaseTermsSaved');
      }
    } catch {
      if (this.isCurrent(access)) this.message.set('unavailable');
    } finally {
      if (this.isCurrent(access)) this.busy.set(false);
    }
  }

  private isCurrent(access?: Access): access is Access {
    return !!access && !access.signal.aborted && access.context === this.api.scopedContext()
      && access.refresh === this.reload() && access.draft === this.draft();
  }

  private path(plan: string) { return `/v1/procurement/proposals/by-plan/${plan}/terms`; }
  private request(access: Access, path: string, body?: string) {
    return fetch(path, {
      method: body === undefined ? 'GET' : 'POST', body,
      headers: {...access.headers, ...(body === undefined ? {} : {'Content-Type':'application/json'})},
      signal:AbortSignal.any([access.signal, AbortSignal.timeout(body === undefined ? 6000 : 10000)]),
      cache:'no-store', redirect:'error',
    });
  }

  private async readTerms(response: Response, draft: PurchaseDraft): Promise<Terms> {
    if (!response.ok) throw new Error('Terms unavailable');
    const terms: Terms = await response.json();
    if (terms.plan_id !== draft.plan_id || terms.proposal_id !== draft.proposal_id || terms.status !== 'draft'
      || terms.currency !== 'EUR') {
      throw new Error('Invalid purchase terms reference');
    }
    return terms;
  }

  private async load(access: Access) {
    try {
      const response = await this.request(access, this.path(access.draft.plan_id));
      if (!this.isCurrent(access)) return;
      if (response.status !== 404) {
        const stored = await this.readTerms(response, access.draft);
        if (!this.isCurrent(access)) return;
        this.stored.set(stored);
        this.edits.set(Object.fromEntries(stored.lines.map(line => [this.label(line), {
          supplier_id:line.supplier.id, unit_price:line.unit_price,
        }])));
        this.suppliers.set([...new Map(stored.lines.map(line => [line.supplier.id, line.supplier])).values()]);
      }
      this.loaded.set(true);
      if (access.canEdit) await this.loadSuppliers(access);
    } catch {
      if (this.isCurrent(access)) this.message.set('unavailable');
    }
  }

  private async loadSuppliers(access: Access, cursor?: string) {
    this.loadingSuppliers.set(true);
    try {
      const parameters = new URLSearchParams({ role:'supplier' });
      if (cursor) parameters.set('after', cursor);
      const response = await this.request(access, `/v1/partners?${parameters}`);
      if (!response.ok) throw new Error('Suppliers unavailable');
      const page: { items: Supplier[]; next_after: string | null } = await response.json();
      if (!this.isCurrent(access)) return;
      this.suppliers.update(previous => [...new Map([...previous, ...page.items]
        .map(supplier => [supplier.id, supplier])).values()]);
      this.next.set(page.next_after);
    } catch {
      if (this.isCurrent(access)) this.message.set('unavailable');
    } finally {
      if (this.isCurrent(access)) this.loadingSuppliers.set(false);
    }
  }
}
