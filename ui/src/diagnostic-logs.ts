import { Component, computed, DestroyRef, inject, signal } from '@angular/core';
import { LucideArrowLeft, LucideHistory, LucideRadio, LucideRefreshCw, LucideScrollText } from '@lucide/angular';
import { ArticleApi } from './article-api';
import { LOG_LIMIT, POLL_MS, logPage, logPath, readPreview, type LogRow } from './diagnostics-preview-api';
import type { Message } from './messages';
import { Preferences } from './preferences';

type Load = 'recent' | 'earlier' | 'next';

@Component({
  selector: 'erp-diagnostic-log-window',
  imports: [LucideArrowLeft, LucideHistory, LucideRadio, LucideRefreshCw],
  template: `
    <div class="actions">
      @if (live()) {
        <button type="button" [disabled]="disabled()" (click)="refresh()">
          <svg lucideRefreshCw aria-hidden="true"></svg>{{ t()('logsRefresh') }}
        </button>
        <button type="button" [disabled]="disabled()" (click)="browse()">
          <svg lucideHistory aria-hidden="true"></svg>{{ t()('logsBrowse') }}
        </button>
      } @else {
        <button type="button" [disabled]="disabled() || !before()" (click)="earlier()">
          <svg lucideArrowLeft aria-hidden="true"></svg>{{ t()('logsEarlier') }}
        </button>
        <button type="button" [disabled]="disabled()" (click)="resume()">
          <svg lucideRadio aria-hidden="true"></svg>{{ t()('logsResume') }}
        </button>
      }
    </div>
    <p>{{ t()(live() ? 'logsLiveMode' : 'logsHistoryMode') }}</p>
    @if (gap()) { <p role="status">{{ t()('logsGap') }}</p> }
    @if (status(); as notice) {
      <p role="status">{{ t()(notice) }}</p>
    } @else {
      <table aria-labelledby="logs-title">
        <thead><tr><th scope="col">{{ t()('logsTime') }}</th><th scope="col">{{ t()('healthService') }}</th>
          <th scope="col">{{ t()('logsSeverity') }}</th><th scope="col">{{ t()('logsEvent') }}</th></tr></thead>
        <tbody>
          @for (row of rows(); track $index) {
            <tr><td>{{ observed(row.observedAt) }}</td><td>{{ t()(row.service) }}</td>
              <td>{{ t()(row.severity) }}</td><td>{{ t()(row.code) }}</td></tr>
          }
        </tbody>
      </table>
    }
  `,
  styles: [`table {width:100%; border-collapse:collapse} td,th {text-align:left; padding:.5rem; overflow-wrap:anywhere}`],
})
class DiagnosticLogWindow {
  private readonly api = inject(ArticleApi);
  private readonly context = this.api.diagnosticsContext();
  private readonly preferences = inject(Preferences);
  private readonly lifetime = new AbortController();
  private timer?: number;
  private after: string | null = null;
  private readonly pending = signal(false);
  private readonly denied = signal(false);
  private readonly state = signal<'loading' | 'unavailable' | 'available'>('loading');
  readonly t = this.preferences.t;
  readonly live = signal(true);
  readonly before = signal<string | null>(null);
  readonly rows = signal<LogRow[]>([]);
  readonly gap = signal(false);
  readonly disabled = computed(() => this.pending() || this.denied());
  readonly status = computed<Message | null>(() => {
    if (this.state() === 'loading') return 'busy';
    if (this.state() === 'unavailable') return 'logsUnavailable';
    return this.rows().length ? null : 'logsEmpty';
  });

  constructor() {
    inject(DestroyRef).onDestroy(() => {
      this.lifetime.abort();
      window.clearTimeout(this.timer);
    });
    this.refresh();
  }

  observed(ms: number) {
    return new Intl.DateTimeFormat(this.preferences.locale(), { dateStyle: 'medium', timeStyle: 'medium' }).format(ms);
  }

  refresh() { if (this.live()) void this.load(this.after === null ? 'recent' : 'next'); }
  earlier() { if (!this.live() && this.before() !== null) void this.load('earlier'); }
  browse() { this.changeMode(false); }
  resume() { this.changeMode(true); }

  private changeMode(live: boolean) {
    if (this.disabled() || this.live() === live) return;
    this.live.set(live);
    this.clearWindow();
    void this.load('recent');
  }

  private clearWindow() {
    this.rows.set([]);
    this.before.set(null);
    this.after = null;
  }

  private async load(kind: Load) {
    const { signal } = this.lifetime;
    const current = () => !signal.aborted && this.api.canReadLogs() && this.context === this.api.diagnosticsContext();
    if (this.disabled() || !current()) return;
    const cursor = kind === 'earlier' ? this.before() : kind === 'next' ? this.after : null;
    const headers = this.api.bearerHeaders();
    window.clearTimeout(this.timer);
    this.pending.set(true);
    if (kind !== 'next') this.state.set('loading');
    try {
      let reply = await readPreview(logPath(cursor), headers, signal);
      if (!current()) return;
      if (cursor !== null && reply.kind === 'gap') {
        this.clearWindow();
        this.gap.set(true);
        this.state.set('loading');
        kind = 'recent';
        reply = await readPreview(logPath(null), headers, signal);
        if (!current()) return;
      }
      const page = reply.kind === 'ok' ? logPage(reply.body) : null;
      if (!page) {
        if (reply.kind === 'denied') this.denied.set(true);
        if (this.after !== null) this.gap.set(true);
        this.clearWindow();
        this.state.set('unavailable');
        return;
      }
      if (page.gap) this.gap.set(true);
      const previous = kind === 'next' && !page.gap ? this.rows() : [];
      // Store only the newest bounded window, not a second copy for history.
      this.rows.set([...previous.slice(Math.max(0, previous.length + page.rows.length - LOG_LIMIT)), ...page.rows]);
      this.before.set(page.before);
      this.after = page.after;
      this.state.set('available');
    } finally {
      if (current()) {
        this.pending.set(false);
        if (this.live() && !this.denied()) this.timer = window.setTimeout(() => this.refresh(), POLL_MS);
      }
    }
  }
}

@Component({
  selector: 'erp-diagnostic-logs',
  imports: [DiagnosticLogWindow, LucideScrollText],
  template: `
    <section class="card" aria-labelledby="logs-title">
      <h2 id="logs-title">{{ t()('diagnosticLogs') }}</h2>
      <p>{{ t()('logsIntro') }}</p>
      <button type="button" [attr.aria-expanded]="open()" aria-controls="diagnostic-log-window" (click)="open.update(toggle)">
        <svg lucideScrollText aria-hidden="true"></svg>{{ t()(open() ? 'logsHide' : 'logsShow') }}
      </button>
      <div id="diagnostic-log-window">
        @if (open()) { <erp-diagnostic-log-window /> }
      </div>
    </section>
  `,
  styles: [`erp-diagnostic-log-window {display:block; margin-block-start:1rem}`],
})
export class DiagnosticLogs {
  readonly t = inject(Preferences).t;
  readonly open = signal(false);
  readonly toggle = (open: boolean) => !open;
}
