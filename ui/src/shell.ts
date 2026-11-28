import { NgComponentOutlet, NgTemplateOutlet } from '@angular/common';
import { FocusMonitor } from '@angular/cdk/a11y';
import { Component, computed, DestroyRef, ElementRef, inject, signal, viewChild } from '@angular/core';
import { takeUntilDestroyed } from '@angular/core/rxjs-interop';
import { NavigationEnd, Router, RouterLink, RouterLinkActive, RouterOutlet } from '@angular/router';
import { LucideBuilding2, LucideCircleHelp, LucidePanelLeft, LucideX } from '@lucide/angular';
import { SHELL_REGISTRATION, permits, visibleModules } from './navigation';
import { Preferences } from './preferences';
import { ArticleApi } from './article-api';

@Component({
  selector: 'app-root',
  imports: [RouterLink, RouterLinkActive, RouterOutlet, NgComponentOutlet, NgTemplateOutlet,
    LucideBuilding2, LucideCircleHelp, LucidePanelLeft, LucideX],
  templateUrl: './shell.html'
})
export class Shell {
  private readonly registration = inject(SHELL_REGISTRATION);
  readonly prefs = inject(Preferences);
  readonly loopback = this.registration.localSetup;
  readonly t = this.prefs.t;
  readonly api = inject(ArticleApi);
  readonly visible = computed(() => visibleModules(this.registration.modules, {
    ...this.registration.access,
    permissions: new Set([
      ...(this.api.grant()?.permissions ?? []).filter(permission => !permission.startsWith('platform.')),
      ...this.api.diagnosticsPermissions(),
    ]),
  }));
  readonly routeData = signal<Record<string, unknown>>({});
  readonly mobileNav = signal(false);
  readonly main = viewChild.required<ElementRef<HTMLElement>>('main');
  readonly panelButton = viewChild<ElementRef<HTMLButtonElement>>('panelButton');
  readonly panelTitle = viewChild<ElementRef<HTMLElement>>('panelTitle');
  readonly secondaryNav = viewChild<ElementRef<HTMLElement>>('secondaryNav');
  readonly mobileToggle = viewChild<ElementRef<HTMLButtonElement>>('mobileToggle');
  readonly current = computed(() => this.visible().find(module => module.id === this.routeData()['module']));
  readonly contextPanel = computed(() => this.registration.panels.find(panel =>
    panel.id === this.routeData()['panel'] && permits(this.registration.access, panel)));
  readonly panelOpen = computed(() => Boolean(this.contextPanel()) && this.prefs.panel() === 'open');
  readonly groups = signal<ReadonlySet<string>>(new Set());
  private readonly router = inject(Router);
  private readonly focus = inject(FocusMonitor);

  constructor() {
    this.router.events.pipe(takeUntilDestroyed(inject(DestroyRef))).subscribe(event => {
      if (!(event instanceof NavigationEnd)) return;
      let route = this.router.routerState.snapshot.root;
      let data = route.data;
      while (route.firstChild) {
        route = route.firstChild;
        data = { ...data, ...route.data };
      }
      this.routeData.set(data);
      this.mobileNav.set(false);
      if (this.router.lastSuccessfulNavigation()?.previousNavigation) {
        this.focus.focusVia(this.main().nativeElement, 'program');
        this.main().nativeElement.scrollIntoView({ block: 'start' });
      }
    });
  }

  toggleNavigation() {
    if (matchMedia('(max-width: 760px)').matches) {
      this.mobileNav.update(open => !open);
      if (this.mobileNav()) requestAnimationFrame(() => this.secondaryNav()?.nativeElement.querySelector('button')?.focus());
    } else {
      this.prefs.secondary.update(value => value === 'open' ? 'closed' : 'open');
    }
  }

  skipToContent(event: Event) {
    event.preventDefault();
    this.main().nativeElement.focus();
  }

  toggleGroup(id: string) {
    this.groups.update(previous => {
      const next = new Set(previous);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  closePanel() {
    this.prefs.panel.set('closed');
    this.panelButton()?.nativeElement.focus();
  }

  togglePanel() {
    if (this.panelOpen()) this.closePanel();
    else {
      this.prefs.panel.set('open');
      requestAnimationFrame(() => this.panelTitle()?.nativeElement.focus());
    }
  }

  closeMobileNavigation(event: Event) {
    if (!this.mobileNav()) return;
    event.preventDefault();
    this.mobileNav.set(false);
    this.mobileToggle()?.nativeElement.focus();
  }
}
