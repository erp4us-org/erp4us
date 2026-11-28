import { Component, DestroyRef, inject, signal } from '@angular/core';
import { ArticleApi } from './article-api';
import { POLL_MS, readPreview, runtimeReport, type RuntimeReport } from './diagnostics-preview-api';
import { Preferences } from './preferences';

@Component({
  selector: 'erp-runtime-diagnostics',
  template: `
    <section class="card" aria-labelledby="runtime-title">
      <h2 id="runtime-title">{{ t()('runtimeInventory') }}</h2>
      <p>{{ t()('runtimeIntro') }}</p>
      @if (report(); as latest) {
        @if (latest.state === 'unavailable') {
          <p role="status">{{ t()('runtimeUnavailable') }}</p>
        } @else {
          @if (latest.state === 'stale') { <p role="status">{{ t()('runtimeStale') }}</p> }
          <p>{{ t()('healthObservedAt') }}: {{ observed(latest.observedAt) }}</p>
          @if (latest.state === 'fresh') {
            <table aria-labelledby="runtime-title">
              <thead><tr><th scope="col">{{ t()('healthService') }}</th><th scope="col">{{ t()('runtimeState') }}</th></tr></thead>
              <tbody>
                @for (row of latest.rows; track row.service) {
                  <tr><th scope="row">{{ t()(row.service) }}</th><td>{{ t()(row.status) }}</td></tr>
                }
              </tbody>
            </table>
          }
        }
      } @else { <p role="status">{{ t()('busy') }}</p> }
    </section>
  `,
  styles: [`table {width:100%; border-collapse:collapse} td,th {text-align:left; padding:.5rem}`],
})
export class RuntimeDiagnostics {
  private readonly api = inject(ArticleApi);
  private readonly context = this.api.diagnosticsContext();
  private readonly preferences = inject(Preferences);
  private readonly lifetime = new AbortController();
  private pollTimer?: number;
  private expiryTimer?: number;
  readonly t = this.preferences.t;
  readonly report = signal<RuntimeReport | null>(null);

  constructor() {
    inject(DestroyRef).onDestroy(() => {
      this.lifetime.abort();
      window.clearTimeout(this.pollTimer);
      window.clearTimeout(this.expiryTimer);
    });
    void this.refresh();
  }

  observed(ms: number) {
    return new Intl.DateTimeFormat(this.preferences.locale(), { dateStyle: 'medium', timeStyle: 'medium' }).format(ms);
  }

  private async refresh() {
    const { signal } = this.lifetime;
    const current = () => !signal.aborted && this.api.canReadRuntime() && this.context === this.api.diagnosticsContext();
    if (!current()) return;
    const reply = await readPreview('/v1/admin/runtime', this.api.bearerHeaders(), signal);
    if (!current()) return;
    const now = Date.now();
    const report: RuntimeReport = reply.kind === 'ok' ? runtimeReport(reply.body, now) : { state: 'unavailable' };
    window.clearTimeout(this.expiryTimer);
    this.report.set(report);
    if (report.state === 'fresh') {
      this.expiryTimer = window.setTimeout(() => {
        this.report.set({ state: 'stale', observedAt: report.observedAt });
      }, report.expiresAt - now);
    }
    if (reply.kind !== 'denied') this.pollTimer = window.setTimeout(() => void this.refresh(), POLL_MS);
  }
}
