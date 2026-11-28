import { Injectable, inject, signal } from '@angular/core';
import { ArticleApi, isLoopback } from './article-api';

interface Settings { issuer: string; clientId: string }
interface Pending { verifier: string; state: string; created: number }
const PENDING = 'erp4us.preview-pkce';
const validRandom = (value: unknown) => typeof value === 'string' && /^[\w-]{43}$/.test(value);
const encode = (bytes: Uint8Array) => btoa(String.fromCharCode(...bytes)).replace(/=/g, '')
  .replace(/\+/g, '-').replace(/\//g, '_');
const random = () => encode(crypto.getRandomValues(new Uint8Array(32)));

@Injectable({ providedIn: 'root' })
export class BrowserIdentity {
  private readonly api = inject(ArticleApi);
  private readonly configuration = signal<Settings | null>(null);
  readonly available = this.configuration.asReadonly();

  async configure(): Promise<Settings | null> {
    if (!isLoopback()) return null;
    try {
      const response = await fetch('/demo-oidc.json', {
        signal: AbortSignal.timeout(5000), cache: 'no-store', redirect: 'error',
      });
      if (!response.ok) return null;
      const config: Partial<Settings> = await response.json();
      // This adapter is exclusively the known loopback demo, not an arbitrary
      // identity-provider URL forwarding tokens or user credentials.
      if (config.issuer !== 'https://127.0.0.1:9443/realms/erp4us-preview'
          || config.clientId !== 'erp4us-workspace' || location.origin !== 'http://127.0.0.1:4200') return null;
      const settings = { issuer: config.issuer, clientId: config.clientId };
      this.configuration.set(settings);
      return settings;
    } catch { return null; }
  }

  async start(): Promise<boolean> {
    const config = this.configuration();
    if (!config) return false;
    try {
      const verifier = random();
      const state = random();
      const challenge = encode(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier))));
      sessionStorage.setItem(PENDING, JSON.stringify({ verifier, state, created: Date.now() }));
      const url = new URL(config.issuer + '/protocol/openid-connect/auth');
      url.search = new URLSearchParams({
        client_id: config.clientId, redirect_uri: location.origin + '/auth/callback',
        response_type: 'code', scope: 'openid', state,
        code_challenge: challenge, code_challenge_method: 'S256',
      }).toString();
      location.assign(url);
      return true;
    } catch { return false; }
  }

  async complete(params: URLSearchParams): Promise<boolean> {
    try {
      const saved = sessionStorage.getItem(PENDING);
      sessionStorage.removeItem(PENDING);
      if (!saved || params.has('error')) return false;
      const pending: Pending = JSON.parse(saved);
      const code = params.get('code');
      const age = Date.now() - pending.created;
      if (!code || code.length > 2048 || !validRandom(pending.state) || !validRandom(pending.verifier)
          || !Number.isFinite(age) || age < 0 || age > 300000 || params.get('state') !== pending.state) return false;
      const config = await this.configure();
      if (!config || (params.has('iss') && params.get('iss') !== config.issuer)) return false;
      const response = await fetch(`${config.issuer}/protocol/openid-connect/token`, {
        method: 'POST', credentials: 'omit', cache: 'no-store', redirect: 'error',
        signal: AbortSignal.timeout(15000),
        body: new URLSearchParams({
          grant_type: 'authorization_code', client_id: config.clientId, code,
          code_verifier: pending.verifier, redirect_uri: location.origin + '/auth/callback',
        }),
      });
      if (!response.ok) return false;
      const result: { access_token?: string; token_type?: string } = await response.json();
      if (!result || typeof result.access_token !== 'string' || result.access_token.length > 8192
          || result.token_type?.toLowerCase() !== 'bearer') return false;
      return await this.api.acceptAccessToken(result.access_token);
    } catch { return false; }
  }

  signOut() {
    this.api.clearCredential();
    const config = this.configuration();
    if (config) {
      const url = new URL(`${config.issuer}/protocol/openid-connect/logout`);
      url.search = new URLSearchParams({
        client_id: config.clientId, post_logout_redirect_uri: location.origin + '/local-access',
      }).toString();
      location.assign(url);
    }
  }
}
