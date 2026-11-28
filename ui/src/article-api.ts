import { computed, Injectable, signal } from '@angular/core';
import type { Message } from './messages';

export interface Registration { sku: string; name: string }
export interface Article extends Registration { id: string; revision: number }
export type Result = { article: Article } | { error: Message };
export const isArticleId = (id: string) => /^[\da-f]{8}-(?:[\da-f]{4}-){3}[\da-f]{12}$/i.test(id);
export const isLoopback = () => ['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);
export const platformDiagnosticsPermissions = ['platform.health.read', 'platform.runtime.read', 'platform.logs.read'] as const;

interface Grant { tenant_id: string; organization_id: string; permissions: string[] }
const fixtureScope = {
  tenant_id: 'ddcae139-c1e8-47d2-8b48-349cb6f30ce5',
  organization_id: '3e793fb5-75a7-45f6-8f66-45a58fcec674',
};

@Injectable({ providedIn: 'root' })
export class ArticleApi {
  private readonly credential = signal('');
  private readonly verifiedGrants = signal<Grant[]>([]);
  private readonly verifiedPlatform = signal<string[]>([]);
  private readonly selected = signal(0);
  private readonly sessionVersion = signal(0);
  private readonly credentialExpired = signal(false);
  private expiresAt = Infinity;
  private expiryTimer?: ReturnType<typeof setTimeout>;
  private pending?: AbortController;
  readonly hasCredential = computed(() => Boolean(this.credential()));
  readonly grants = this.verifiedGrants.asReadonly();
  readonly grant = computed(() => this.grants()[this.selected()]);
  readonly selectedIndex = this.selected.asReadonly();
  readonly canPlan = computed(() => this.grant()?.permissions.includes('manufacturing.plan.read') ?? false);
  readonly canReadPartners = computed(() => this.grant()?.permissions.includes('master-data.partner.read') ?? false);
  readonly canReadSales = computed(() => this.grant()?.permissions.includes('sales.order.read') ?? false);
  readonly canConfirmSales = computed(() => this.grant()?.permissions.includes('sales.order.confirm') ?? false);
  readonly canReadReservation = computed(() => this.grant()?.permissions.includes('inventory.reservation.read') ?? false);
  readonly canReadMaterialPlan = computed(() => this.grant()?.permissions.includes('inventory.material-plan.read') ?? false);
  readonly canCreateMaterialPlan = computed(() => this.canPlan()
    && (this.grant()?.permissions.includes('inventory.material-plan.create') ?? false));
  readonly canSendMaterialPlan = computed(() => this.grant()?.permissions.includes('inventory.material-plan.send') ?? false);
  readonly canReadProposal = computed(() => this.grant()?.permissions.includes('procurement.proposal.read') ?? false);
  readonly canReadPurchaseOrders = computed(() => this.grant()?.permissions.includes('procurement.purchase-order.read') ?? false);
  readonly freshCredential = computed(() => !this.credentialExpired() && this.hasCredential());
  readonly canDiscoverReceiptSource = computed(() => this.freshCredential()
    && (this.grant()?.permissions.includes('procurement.purchase-order.receipt-source.read') ?? false));
  readonly canReadReceiptAllocation = computed(() => this.freshCredential()
    && (this.grant()?.permissions.includes('inventory.purchase-receipt.read') ?? false));
  readonly canPostPurchaseReceipt = computed(() => this.freshCredential()
    && (this.grant()?.permissions.includes('inventory.purchase-receipt.post') ?? false));
  readonly canIssuePurchaseOrders = computed(() => this.freshCredential() && this.canReadPurchaseOrders()
    && (this.grant()?.permissions.includes('procurement.purchase-order.issue') ?? false));
  readonly canEditProposalTerms = computed(() => this.canReadProposal() && this.canReadPartners()
    && (this.grant()?.permissions.includes('procurement.proposal.terms.write') ?? false));
  readonly canCreateSalesDraft = computed(() => this.canReadPartners() && this.canPlan()
    && (this.grant()?.permissions.includes('sales.order.draft.create') ?? false));
  readonly diagnosticsPermissions = computed(() =>
    platformDiagnosticsPermissions.filter(permission => this.verifiedPlatform().includes(permission)));
  readonly canReadHealth = computed(() => this.diagnosticsPermissions().includes('platform.health.read'));
  readonly canReadRuntime = computed(() => this.diagnosticsPermissions().includes('platform.runtime.read'));
  readonly canReadLogs = computed(() => this.diagnosticsPermissions().includes('platform.logs.read'));
  readonly credentialGeneration = this.sessionVersion.asReadonly();
  readonly scopedContext = computed(() => [this.sessionVersion(), this.selected()] as const);
  readonly diagnosticsContext = this.scopedContext;

  setCredential(value: string) {
    if (!isLoopback() || !value.trim()) return false;
    this.clearCredential();
    this.setSessionToken(value.trim());
    void this.readGrants(value.trim());
    return true;
  }

  async acceptAccessToken(token: string): Promise<boolean> {
    if (!isLoopback() || !token.trim()) return false;
    this.clearCredential();
    this.setSessionToken(token);
    if (await this.readGrants(token)) return true;
    if (this.credential() === token) this.clearCredential();
    return false;
  }

  clearCredential() {
    this.pending?.abort();
    clearTimeout(this.expiryTimer);
    this.expiresAt = Infinity;
    this.credentialExpired.set(false);
    this.sessionVersion.update(version => version + 1);
    this.credential.set('');
    this.verifiedGrants.set([]);
    this.verifiedPlatform.set([]);
    this.selected.set(0);
  }
  credentialStillCurrent() { return this.hasCredential() && Date.now() < this.expiresAt; }

  private setSessionToken(token: string) {
    this.credential.set(token);
    // Only suppress expired-token UI. The server still verifies the token and
    // grants; claims read here never grant access. Opaque fixture tokens work too.
    try {
      const part = token.split('.')[1];
      const {exp} = JSON.parse(atob(part.replace(/-/g, '+').replace(/_/g, '/')));
      if (typeof exp !== 'number' || !Number.isSafeInteger(exp) || !Number.isSafeInteger(exp * 1000)) return;
      this.expiresAt = exp * 1000;
      this.watchExpiry();
    } catch { /* No local expiry information. */ }
  }

  private watchExpiry() {
    const delay = this.expiresAt - Date.now();
    if (delay <= 0) this.credentialExpired.set(true);
    else this.expiryTimer = setTimeout(() => this.watchExpiry(), Math.min(delay, 2_147_483_647));
  }
  selectGrant(index: number) {
    if (this.grants()[index]) this.selected.set(index);
  }
  scopeLabel(grant: Grant) {
    return grant.tenant_id === fixtureScope.tenant_id && grant.organization_id === fixtureScope.organization_id
      ? 'NovaMotion' : `${grant.tenant_id.slice(0, 8)} / ${grant.organization_id.slice(0, 8)}`;
  }
  requestHeaders(): Record<string, string> {
    const scope = this.grant() ?? fixtureScope;
    return {
      ...this.bearerHeaders(),
      'X-Erp4us-Tenant': scope.tenant_id,
      'X-Erp4us-Organization': scope.organization_id,
    };
  }
  bearerHeaders() {
    return { Authorization: `Bearer ${this.credential()}` };
  }
  private async readGrants(token: string): Promise<boolean> {
    const pending = new AbortController();
    this.pending = pending;
    try {
      const response = await fetch('/v1/session', {
        headers: { Authorization: `Bearer ${token}` }, cache: 'no-store', redirect: 'error',
        signal: AbortSignal.any([pending.signal, AbortSignal.timeout(5000)]),
      });
      if (!response.ok) return false;
      const principal: { grants?: Grant[]; platform_permissions?: string[] } = await response.json();
      if (!pending.signal.aborted && principal && Array.isArray(principal.grants)) {
        this.verifiedGrants.set(principal.grants.filter(grant => grant && isArticleId(grant.tenant_id)
          && isArticleId(grant.organization_id) && Array.isArray(grant.permissions)
          && grant.permissions.every(permission => typeof permission === 'string')));
        if (Array.isArray(principal.platform_permissions)) {
          this.verifiedPlatform.set(principal.platform_permissions.filter(permission => typeof permission === 'string'));
        }
        return this.verifiedGrants().length > 0 || this.verifiedPlatform().length > 0;
      }
    } catch {
      // No verified grants are exposed on failure. The original local fixture
      // flow may still display a request error from the owning service.
    }
    return false;
  }
  register(article: Registration) { return this.request('/v1/articles:register', article); }
  get(id: string, signal?: AbortSignal): Promise<Result> {
    return isArticleId(id) ? this.request(`/v1/articles/${id}`, undefined, signal) : Promise.resolve({ error: 'invalidId' });
  }

  private async request(path: string, body?: Registration, signal?: AbortSignal): Promise<Result> {
    if (!isLoopback() || !this.credential()) return { error: 'tokenMissing' };
    const timeLimit = AbortSignal.timeout(15000);
    try {
      const response = await fetch(path, {
        method: body ? 'POST' : 'GET',
        headers: {
          ...this.requestHeaders(),
          ...(body ? { 'Content-Type': 'application/json' } : {}),
        },
        body: body ? JSON.stringify(body) : undefined,
        redirect: 'error', cache: 'no-store', signal: signal ? AbortSignal.any([signal, timeLimit]) : timeLimit
      });
      if (!response.ok) {
        const errors: Partial<Record<number, Message>> = {
          400: body ? 'invalid' : 'invalidId', 401: 'denied', 403: 'denied', 404: 'missing', 409: 'duplicate', 422: 'invalid'
        };
        return { error: errors[response.status] ?? 'unavailable' };
      }
      const article: Partial<Article> | null = await response.json();
      if (!article || typeof article.id !== 'string' || !isArticleId(article.id) ||
          typeof article.sku !== 'string' || typeof article.name !== 'string' ||
          typeof article.revision !== 'number' || !Number.isSafeInteger(article.revision)) return { error: 'unavailable' };
      return { article: { id: article.id, sku: article.sku, name: article.name, revision: article.revision } };
    } catch {
      return { error: 'unavailable' };
    }
  }
}
