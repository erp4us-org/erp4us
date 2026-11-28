import { bootstrapApplication } from '@angular/platform-browser';
import { provideRouter, type Routes } from '@angular/router';
import { LucideActivity, LucideBoxes, LucideFactory, LucideLayoutDashboard, LucideShoppingCart } from '@lucide/angular';
import { Shell } from './shell';
import { Workspace, DemoAccess } from './workspace';
import { isLoopback, platformDiagnosticsPermissions } from './article-api';
import { Products, ProductRegistration, ProductDetail, ProductGuide, confirmLeave } from './products';
import { SHELL_REGISTRATION, type ShellRegistration } from './navigation';
import { Manufacturing } from './manufacturing';
import { SignInCallback } from './sign-in-callback';
import { Partners } from './partners';
import { ApplicationDiagnostics } from './application-diagnostics';
import { SalesOrders } from './sales-orders';
import { OrderReservation } from './order-reservation';

const registration: ShellRegistration = {
  modules: [
    { schemaVersion: 1, id: 'workspace', label: 'workspace', icon: LucideLayoutDashboard, route: '/', groups: [] },
    { schemaVersion: 1, id: 'master-data', label: 'masterData', icon: LucideBoxes, route: '/products', groups: [
      { id: 'catalog', label: 'catalog', entries: [
        { id: 'products', label: 'products', route: '/products' },
        { id: 'partners', label: 'partners', route: '/partners', permissions: ['master-data.partner.read'] },
      ] }
    ] },
    { schemaVersion: 1, id: 'manufacturing', label: 'manufacturing', icon: LucideFactory,
      route: '/manufacturing', groups: [], permissions: ['manufacturing.plan.read'] },
    { schemaVersion: 1, id: 'sales', label: 'salesOrders', icon: LucideShoppingCart,
      route: '/sales/orders', groups: [], permissions: ['sales.order.read'] },
    { schemaVersion: 1, id: 'administration', label: 'administration', icon: LucideActivity,
      route: '/administration', groups: [], anyPermission: platformDiagnosticsPermissions }
  ],
  panels: [{ id: 'product-guide', title: 'productGuide', component: ProductGuide }],
  access: {
    installed: new Set(['workspace', 'master-data', 'manufacturing', 'administration', 'sales']),
    enabled: new Set(['workspace', 'master-data', 'manufacturing', 'administration', 'sales']),
    permissions: new Set()
  },
  localSetup: isLoopback()
};

const routes: Routes = [
  { path: '', component: Workspace, data: { module: 'workspace' } },
  { path: 'products', data: { module: 'master-data' }, children: [
    { path: '', component: Products, data: { panel: 'product-guide' } },
    { path: 'new', component: ProductRegistration, canDeactivate: [confirmLeave], data: { panel: 'product-guide' } },
    { path: ':id', component: ProductDetail, data: { panel: 'product-guide' } }
  ] },
  { path: 'local-access', component: DemoAccess, canMatch: [isLoopback] },
  { path: 'auth/callback', component: SignInCallback, canMatch: [isLoopback] },
  { path: 'partners', component: Partners, data: {module: 'master-data'} },
  { path: 'manufacturing', component: Manufacturing, data: { module: 'manufacturing' } },
  { path: 'sales/orders', component: SalesOrders, data: { module: 'sales' } },
  { path: 'sales/orders/:orderId/reservations/:lineId', component: OrderReservation, data: { module: 'sales' } },
  { path: 'administration', component: ApplicationDiagnostics, data: { module: 'administration' } },
  { path: '**', redirectTo: '' }
];

bootstrapApplication(Shell, { providers: [
  provideRouter(routes), { provide: SHELL_REGISTRATION, useValue: registration }
] })
  .catch(() => console.error('Workspace startup failed.'));
