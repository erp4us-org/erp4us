import { expect, test, type Page, type Route } from '@playwright/test';

const runtime = 'platform.runtime.read';
const logs = 'platform.logs.read';
const health = 'platform.health.read';
const tenantGrant = {
  tenant_id: 'ddcae139-c1e8-47d2-8b48-349cb6f30ce5',
  organization_id: '3e793fb5-75a7-45f6-8f66-45a58fcec674',
  permissions: ['tenant.admin', health, runtime, logs], // Tenant claims must never open platform panels.
};
const sessions = {
  tenant: { grants: [tenantGrant], platform_permissions: [] },
  monitor: { grants: [], platform_permissions: [health] },
  runtime: { grants: [], platform_permissions: [runtime] },
  logs: { grants: [], platform_permissions: [logs] },
  operator: {
    grants: [tenantGrant, { ...tenantGrant, organization_id: 'f9503f4e-3031-4759-8386-468f5eb2ae57' }],
    platform_permissions: [runtime, logs],
  },
};
const unknownRuntime = { state: 'unavailable', observed_at_unix_ms: null, expires_at_unix_ms: null, services: null };
const logWindow = (records: unknown[], after: string, before: string | null = null, gap = false) => ({
  state: 'available', page: { records, after_cursor: after, before_cursor: before, gap_before: gap },
});
const relay = {
  observed_at_unix_ms: Date.UTC(2026, 0, 1), service: 'article-relay', severity: 'info', code: 'relay_connected',
};
const plan = { ...relay, service: 'manufacturing', severity: 'error', code: 'plan_read_unavailable' };
const canary = '<img alt="diagnostic-fixture-secret" src=x>';
declare global {
  interface Window { fixtureDiagnosticAborts: string[] }
}

async function useCredential(page: Page, principal: keyof typeof sessions) {
  await page.getByRole('banner').getByRole('link', { name: 'Local demo access' }).click();
  await page.getByLabel('Demo token').fill(principal);
  const verified = page.waitForResponse(response => response.url().endsWith('/v1/session'));
  await page.getByRole('button', { name: 'Use for this session' }).click();
  expect((await verified).ok()).toBe(true);
}

test('proposed platform diagnostics are independent, bounded and cleared with the view', async ({ page }) => {
  const requests: { principal: string; path: string; cursor: string | null }[] = [];
  let runtimeBody: () => Promise<unknown> = async () => unknownRuntime;
  let logResponse = (route: Route, _cursor: string | null) =>
    route.fulfill({ json: { state: 'unavailable', page: null } });
  await page.clock.install();
  await page.addInitScript(() => {
    // Chromium can defer requestfailed for intercepted requests until the fixture route releases.
    // Observe the actual fetch signal without replacing its behavior or forcing cancellation.
    window.fixtureDiagnosticAborts = [];
    const fetch = window.fetch.bind(window);
    window.fetch = (input, init) => {
      if (typeof input === 'string' && input.endsWith('cursor=scope_tail') && init?.signal) {
        const signal = init.signal;
        signal.addEventListener('abort', () => window.fixtureDiagnosticAborts.push(signal.reason.name), { once: true });
      }
      return fetch(input, init);
    };
  });
  await page.route('**/demo-oidc.json', route => route.fulfill({ status: 404 }));
  await page.route('**/v1/**', async route => {
    const request = route.request();
    const { pathname: path, searchParams } = new URL(request.url());
    const headers = request.headers();
    const principal = headers['authorization']?.replace(/^Bearer /, '') as keyof typeof sessions;
    if (path === '/v1/session') return route.fulfill({ json: sessions[principal] });
    const cursor = searchParams.get('cursor');
    requests.push({ principal, path, cursor });
    expect(request.method()).toBe('GET');
    expect(headers['x-erp4us-tenant']).toBeUndefined();
    expect(headers['x-erp4us-organization']).toBeUndefined();
    if (path === '/v1/admin/application-health' && principal === 'monitor') {
      return route.fulfill({ json: {
        observed_at_unix_ms: relay.observed_at_unix_ms,
        application_checks: [{ service: 'manufacturing', readiness: 'unreachable' }],
        jetstream: { stream: null, consumer: null }, article_relay_heartbeat: { state: 'disabled' },
      } });
    }
    if (path === '/v1/admin/runtime' && (principal === 'runtime' || principal === 'operator')) {
      expect([...searchParams]).toEqual([]);
      return route.fulfill({ json: await runtimeBody() });
    }
    if (path === '/v1/admin/diagnostic-logs' && (principal === 'logs' || principal === 'operator')) {
      expect([...searchParams]).toEqual(cursor === null ? [['limit', '100']] : [['limit', '100'], ['cursor', cursor]]);
      return logResponse(route, cursor);
    }
    // Nothing from this test reaches a real ERP service, including erroneous requests.
    return route.fulfill({ status: 403 });
  });
  const admin = page.getByRole('link', { name: 'Administration', exact: true });
  const diagnosticLogs = page.getByRole('region', { name: /Diagnostic logs \(preview\)|Diagnoseprotokoll \(Vorschau\)/ });
  const runtimeInventory = page.getByRole('region', { name: 'Runtime inventory (preview)' });
  const open = () => diagnosticLogs.getByRole('button', { name: 'Show diagnostic logs' }).press('Enter');
  const refresh = () => diagnosticLogs.getByRole('button', { name: 'Refresh logs' }).click();
  const tick = () => page.clock.fastForward(6100);

  await test.step('only platform grants reveal and request their own panels', async () => {
    await page.goto('/administration');
    await useCredential(page, 'tenant');
    await expect(page.getByLabel('Company and organization')).toBeVisible();
    await page.goBack();
    await expect(page.getByText('A platform diagnostics permission is required.')).toBeVisible();
    await expect(admin).toHaveCount(0);
    await expect(runtimeInventory.or(diagnosticLogs)).toHaveCount(0);
    await tick();
    expect(requests).toEqual([]);

    await useCredential(page, 'monitor');
    await admin.click();
    await expect(page.getByRole('cell', { name: 'Not responding' })).toBeVisible();
    await expect(page.getByText('Heartbeats disabled')).toBeVisible();
    await expect(runtimeInventory.or(diagnosticLogs)).toHaveCount(0);
    await tick();
    expect(requests.filter(r => r.principal === 'monitor').map(r => r.path)).not.toContain('/v1/admin/runtime');
    expect(requests.every(r => r.path === '/v1/admin/application-health')).toBe(true);
  });

  await test.step('runtime distinguishes current observations, expiry and no usable scan', async () => {
    await useCredential(page, 'runtime');
    await admin.click();
    await expect(runtimeInventory).toContainText('Runtime observation unavailable');
    await expect(runtimeInventory.getByRole('table')).toHaveCount(0);
    await expect(diagnosticLogs).toHaveCount(0);
    const fresh = async () => {
      const now = await page.evaluate(() => Date.now());
      return {
        state: 'fresh', observed_at_unix_ms: now, expires_at_unix_ms: now + 4000,
        services: { 'master-data': 'present_running', manufacturing: 'present_exited', 'article-relay': 'absent' },
      };
    };
    runtimeBody = fresh;
    await tick();
    await expect(runtimeInventory.getByRole('row', { name: /Master data/ })).toContainText('Present, running');
    await expect(runtimeInventory.getByRole('row', { name: /Manufacturing/ })).toContainText('Present, exited');
    await expect(runtimeInventory.getByRole('row', { name: /Article Relay/ })).toContainText('Absent from the observed project');
    await page.clock.fastForward(4100);
    await expect(runtimeInventory).toContainText('Runtime observation is stale');
    await expect(runtimeInventory.getByRole('table')).toHaveCount(0);
    runtimeBody = async () => ({ ...await fresh(), services: { manufacturing: 'absent' } });
    await tick();
    await expect(runtimeInventory).toContainText('Runtime observation unavailable');
    await expect(runtimeInventory.getByRole('table')).toHaveCount(0);
    expect(requests.filter(r => r.principal === 'runtime').every(r => r.path === '/v1/admin/runtime')).toBe(true);
  });

  await test.step('logs distinguish null from empty, sanitize rows, page history and recover from a cursor gap', async () => {
    await useCredential(page, 'logs');
    await admin.click();
    await expect(runtimeInventory).toHaveCount(0);
    await tick();
    expect(requests.filter(r => r.principal === 'logs')).toHaveLength(0);
    await open();
    await expect(diagnosticLogs).toContainText('Diagnostic logs unavailable');
    logResponse = route => route.fulfill({ json: logWindow([], 'empty') });
    await refresh();
    await expect(diagnosticLogs).toContainText('No diagnostic events in this window');

    logResponse = (route, cursor) => {
      expect(cursor).toBe('empty');
      return route.fulfill({ json: logWindow([
        { ...relay, id: canary, payload: canary, message: canary },
        { ...plan, code: canary }, { ...plan, severity: canary }, { ...plan, service: canary },
        { ...relay, service: 'manufacturing' }, { ...relay, severity: '__proto__' },
      ], 'tail_one') });
    };
    const logTable = diagnosticLogs.getByRole('table');
    await tick(); // Poll automatically only after it was opened.
    await expect(logTable.getByRole('row')).toHaveCount(2);
    await expect(logTable).toContainText('Relay connected');
    await expect(logTable).not.toContainText('diagnostic-fixture-secret');
    await expect(logTable.locator('img')).toHaveCount(0);
    await page.getByLabel('Language', { exact: true }).selectOption('de');
    await expect(logTable.getByRole('cell', { name: 'Relay verbunden' })).toBeVisible();
    await page.getByLabel('Sprache', { exact: true }).selectOption('en');

    logResponse = (route, cursor) => {
      expect(cursor).toBe('tail_one');
      return route.fulfill({ json: logWindow(Array.from({ length: 100 }, () => plan), 'tail_two') });
    };
    await refresh();
    await expect(logTable.getByRole('row')).toHaveCount(101);
    await expect(logTable).not.toContainText('Relay connected');

    logResponse = (route, cursor) => {
      if (cursor === 'tail_two') return route.fulfill({ status: 410 });
      if (cursor === 'back_one') return route.fulfill({ json: logWindow([plan], 'older_tail') });
      expect(cursor).toBeNull();
      return route.fulfill({ json: logWindow([relay], 'tail_three', 'back_one') });
    };
    await refresh();
    await expect(diagnosticLogs).toContainText('Log continuity could not be confirmed.');
    await expect(logTable.getByRole('row')).toHaveCount(2);
    await expect(logTable).not.toContainText('Planning read unavailable');
    await diagnosticLogs.getByRole('button', { name: 'Browse history' }).click();
    await diagnosticLogs.getByRole('button', { name: 'Load earlier records' }).click();
    await expect(logTable).toContainText('Planning read unavailable');
    await expect(diagnosticLogs.getByRole('button', { name: 'Load earlier records' })).toBeDisabled();
    const count = requests.length;
    await tick();
    expect(requests).toHaveLength(count);
    expect(requests.filter(r => r.principal === 'logs').every(r => r.path === '/v1/admin/diagnostic-logs')).toBe(true);

    await diagnosticLogs.getByRole('button', { name: 'Return to live logs' }).click();
    await expect(logTable).toContainText('Relay connected');
    logResponse = route => route.fulfill({ json: { ...logWindow([plan], 'huge_body'), raw_field: 'X'.repeat(65536) } });
    await refresh();
    await expect(diagnosticLogs).toContainText('Diagnostic logs unavailable');
    await expect(logTable).toHaveCount(0);
    logResponse = route => route.fulfill({ json: logWindow([relay], 'bounded_body') });
    await refresh();
    await expect(logTable).toContainText('Relay connected');
    logResponse = route => route.fulfill({ json: logWindow(Array.from({ length: 101 }, () => plan), 'oversize') });
    await refresh();
    await expect(diagnosticLogs).toContainText('Diagnostic logs unavailable');
    await expect(logTable).toHaveCount(0);
    await diagnosticLogs.getByRole('button', { name: 'Hide diagnostic logs' }).press('Enter');
    await expect(diagnosticLogs.getByRole('button', { name: 'Refresh logs' })).toHaveCount(0);
    const closedCount = requests.length;
    await tick();
    expect(requests).toHaveLength(closedCount);
  });

  await test.step('scope change aborts a pending read and closes logs; credential clearing prevents any restart', async () => {
    runtimeBody = async () => unknownRuntime;
    logResponse = route => route.fulfill({ json: logWindow([relay], 'scope_tail') });
    await useCredential(page, 'operator');
    await admin.click();
    await open();
    await expect(diagnosticLogs).toContainText('Relay connected');
    await page.clock.fastForward(4500);
    const hold = Promise.withResolvers<void>();
    logResponse = async route => {
      await hold.promise;
      await route.fulfill({ json: logWindow([plan], 'late_response') }).catch(() => {});
    };
    try {
      const active = page.waitForRequest(req => req.url().endsWith('cursor=scope_tail'));
      await refresh();
      await active;
      await expect(diagnosticLogs.getByRole('button', { name: 'Refresh logs' })).toBeDisabled();
      const count = requests.filter(r => r.principal === 'operator' && r.cursor === 'scope_tail').length;
      // Cross the old poll deadline while the manual request is still before its timeout.
      await page.clock.fastForward(2500);
      expect(requests.filter(r => r.principal === 'operator' && r.cursor === 'scope_tail')).toHaveLength(count);
      await page.getByLabel('Company and organization').selectOption('1');
      await expect.poll(() => page.evaluate(() => window.fixtureDiagnosticAborts)).toEqual(['AbortError']);
      await expect(diagnosticLogs.getByRole('button', { name: 'Show diagnostic logs' })).toBeVisible();
      await expect(diagnosticLogs).not.toContainText('Relay connected');
    } finally {
      hold.resolve();
    }
    const logCount = requests.filter(r => r.path === '/v1/admin/diagnostic-logs').length;
    await tick();
    expect(requests.filter(r => r.path === '/v1/admin/diagnostic-logs')).toHaveLength(logCount);

    await page.getByRole('banner').getByRole('link', { name: 'Local demo access' }).click();
    await page.getByRole('button', { name: 'Clear session credential' }).click();
    await expect(admin).toHaveCount(0);
    await page.goBack();
    await expect(runtimeInventory.or(diagnosticLogs)).toHaveCount(0);
    const count = requests.length;
    await tick();
    expect(requests).toHaveLength(count);
  });
});
