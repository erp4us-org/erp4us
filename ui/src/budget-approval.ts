import { Component, computed, effect, inject, input, signal } from '@angular/core';
import { LucideCheck, LucideRefreshCw } from '@lucide/angular';
import { ArticleApi, isArticleId } from './article-api';
import type { Message } from './messages';
import { Preferences } from './preferences';
import { Feedback } from './ui';

type StageState = 'pending' | 'approved';
interface Stage {key: string; state: StageState}
export interface BudgetApproval {
  proposal_id: string; plan_id: string; terms_revision: number;
  policy_id: string; policy_revision: number; currency: 'EUR'; total: string;
  state: StageState; stages: Stage[];
}
interface Access {
  path: string; plan: string; proposal: string; revision: number;
  headers: Record<string, string>; signal: AbortSignal;
  context: ReturnType<ArticleApi['scopedContext']>; refresh: number;
}
const stageKey = /^[a-z][a-z0-9-]{0,31}(?![\s\S])/;

@Component({
  selector: 'erp-budget-approval',
  imports: [Feedback, LucideCheck, LucideRefreshCw],
  template: `
    <section [attr.aria-labelledby]="'budget-approval-' + planId()">
      <h4 [id]="'budget-approval-' + planId()">{{ t()('budgetApproval') }}</h4>
      <p>{{ t()('budgetApprovalIntro') }}</p>
      <erp-feedback [message]="message() ? t()(message()!) : ''" />
      @if (current(); as approval) {
        <p>{{ t()('budgetApprovalBasis') }}: {{ approval.currency }} {{ approval.total }} ·
          {{ t()('purchaseTermsRevision') }} {{ approval.terms_revision }}</p>
        <ol>
          @for (stage of approval.stages; track stage.key) {
            <li>{{ name(stage.key) }} · {{ t()(stage.state === 'approved' ? 'budgetStageApproved' : 'budgetStagePending') }}</li>
          }
        </ol>
        @if (approval.state === 'approved') {
          <p>{{ t()('budgetApprovalReady') }}</p>
        } @else if (next(); as stage) {
          @if (!locked() && canApprove(stage.key)) {
            <button type="button" (click)="approve()" [disabled]="busy() || stale()">
              <svg lucideCheck aria-hidden="true"></svg>{{ t()('budgetApproveStage') }}: {{ name(stage.key) }}
            </button>
          }
        }
      }
      <button type="button" (click)="reload.update(n => n + 1)" [disabled]="busy()">
        <svg lucideRefreshCw aria-hidden="true"></svg>{{ t()('budgetRefresh') }}
      </button>
    </section>
  `,
})
export class BudgetApprovalPanel {
  readonly planId = input.required<string>();
  readonly proposalId = input.required<string>();
  readonly termsRevision = input.required<number>();
  readonly locked = input(false);
  readonly api = inject(ArticleApi);
  readonly t = inject(Preferences).t;
  readonly current = signal<BudgetApproval | null>(null);
  readonly busy = signal(false);
  readonly stale = signal(false);
  readonly message = signal<Message | null>(null);
  readonly reload = signal(0);
  readonly next = computed(() => this.current()?.stages.find(stage => stage.state === 'pending'));
  readonly approved = computed(() => {
    const approval = this.current();
    return approval?.state === 'approved' && !this.busy() && !this.stale() && !this.message()
      && this.isCurrent(this.access) ? approval : null;
  });
  private access?: Access;
  private retry?: {intent: string; id: string};

  constructor() {
    effect(onCleanup => {
      const refresh = this.reload(), context = this.api.scopedContext();
      const plan = this.planId(), proposal = this.proposalId(), revision = this.termsRevision();
      const allowed = this.api.canReadProposal();
      const headers = this.api.requestHeaders();
      this.access = undefined;
      this.retry = undefined;
      this.current.set(null);
      this.busy.set(false);
      this.stale.set(false);
      this.message.set(null);
      if (!allowed || !isArticleId(plan) || !isArticleId(proposal) || !Number.isSafeInteger(revision) || revision < 1) return;
      const control = new AbortController();
      onCleanup(() => control.abort());
      this.access = {
        path: `/v1/procurement/proposals/by-plan/${plan}/approval`,
        plan, proposal, revision, headers, signal: control.signal, context, refresh,
      };
      void this.update(this.access);
    });
  }

  name(key: string) {
    if (key === 'department') return this.t()('budgetStageDepartment');
    if (key === 'budget') return this.t()('budgetStageBudget');
    return key;
  }

  canApprove(key: string) {
    return stageKey.test(key) && (this.api.grant()?.permissions.includes(`procurement.approval.${key}`) ?? false);
  }

  approve() {
    const stage = this.next(), approval = this.current(), access = this.access;
    if (!stage || !approval || !this.isCurrent(access) || this.locked() || this.busy() || this.stale()
      || !this.canApprove(stage.key)) return;
    const intent = {
      terms_revision: access.revision, policy_id: approval.policy_id,
      policy_revision: approval.policy_revision, stage_key: stage.key,
    };
    const key = JSON.stringify(intent);
    if (this.retry?.intent !== key) this.retry = {intent: key, id: crypto.randomUUID()};
    void this.update(access, JSON.stringify({command_id: this.retry.id, ...intent}));
  }

  private isCurrent(access?: Access): access is Access {
    return !!access && !access.signal.aborted && access.context === this.api.scopedContext()
      && access.refresh === this.reload() && access.plan === this.planId() && access.proposal === this.proposalId()
      && access.revision === this.termsRevision();
  }

  private async update(access: Access, body?: string) {
    const {signal, headers, path} = access;
    this.busy.set(true);
    this.message.set('busy');
    try {
      const response = await fetch(path, {
        method: body === undefined ? 'GET' : 'POST', body,
        headers: {...headers, ...(body === undefined ? {} : {'Content-Type': 'application/json'})},
        signal: AbortSignal.any([signal, AbortSignal.timeout(body === undefined ? 6000 : 10000)]),
        cache: 'no-store', redirect: 'error',
      });
      if (!this.isCurrent(access)) return;
      if (response.status === 403) {
        this.message.set('budgetApprovalDenied');
      } else if (response.status === 409) {
        this.stale.set(true);
        this.message.set('budgetApprovalStale');
      } else if (!response.ok) {
        this.message.set('budgetApprovalUnavailable');
      } else {
        const approval: BudgetApproval = await response.json();
        if (!this.isCurrent(access)) return;
        if (approval.plan_id !== access.plan || approval.proposal_id !== access.proposal
          || !isArticleId(approval.policy_id) || approval.currency !== 'EUR'
          || !Number.isSafeInteger(approval.policy_revision) || approval.policy_revision < 1
          || !Array.isArray(approval.stages) || approval.stages.length < 1 || approval.stages.length > 8
          || approval.stages.some(stage => !stage || typeof stage.key !== 'string'
            || !stageKey.test(stage.key) || !['pending', 'approved'].includes(stage.state))
          || new Set(approval.stages.map(stage => stage.key)).size !== approval.stages.length
          || approval.state !== (approval.stages.every(stage => stage.state === 'approved') ? 'approved' : 'pending')) {
          throw new Error('Invalid budget approval view');
        }
        if (approval.terms_revision !== access.revision) {
          this.stale.set(true);
          this.message.set('budgetApprovalStale');
          return;
        }
        this.current.set(approval);
        if (body !== undefined) this.retry = undefined;
        this.message.set(null);
      }
    } catch {
      if (this.isCurrent(access)) this.message.set('budgetApprovalUnavailable');
    } finally {
      if (this.isCurrent(access)) this.busy.set(false);
    }
  }
}
