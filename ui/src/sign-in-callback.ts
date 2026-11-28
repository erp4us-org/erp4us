import { Component, inject, signal } from '@angular/core';
import { Router, RouterLink } from '@angular/router';
import { BrowserIdentity } from './browser-identity';
import { Preferences } from './preferences';
import { Feedback, PageHeading } from './ui';

@Component({
  imports: [Feedback, PageHeading, RouterLink],
  template: `
    <erp-page-heading [title]="t()('signIn')" />
    <erp-feedback [message]="failed() ? t()('signInFailed') : t()('busy')" />
    @if (failed()) { <a routerLink="/local-access">{{ t()('access') }}</a> }
  `,
})
export class SignInCallback {
  readonly t = inject(Preferences).t;
  private readonly identity = inject(BrowserIdentity);
  private readonly router = inject(Router);
  readonly failed = signal(false);

  constructor() {
    const params = new URLSearchParams(location.search);
    // Remove the authorization code from the visible URL before networking.
    history.replaceState(history.state, '', '/auth/callback');
    void this.identity.complete(params).then(ok => {
      if (ok) void this.router.navigateByUrl('/', { replaceUrl: true });
      else this.failed.set(true);
    });
  }
}
