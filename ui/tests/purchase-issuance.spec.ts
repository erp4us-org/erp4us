import { expect, test, type Page, type Route } from '@playwright/test';

const uuid = (index: number) => `a0000000-0000-4000-8000-${index.toString(16).padStart(12, '0')}`;
const [tenant, organization, otherOrg, order, line, plan, proposal, policy, batchId, supplierA, supplierB, poA, poB, poLineA, poLineB]
  = Array.from({ length: 15 }, (_, i) => uuid(i + 1));
const reservation = `/v1/inventory/reservations/${order}/${line}`;
const proposalPath = `/v1/procurement/proposals/by-plan/${plan}`;
const purchasePath = `${proposalPath}/purchase-orders`;
const pagePath = `/sales/orders/${order}/reservations/${line}`;
const draftLines = [
  { sku: 'NM-ENDPLATE', revision: null, unit: 'EA', pieces: '12' },
  { sku: 'NM-MOTOR', revision: 'A', unit: 'EA', pieces: '10' },
];
const suppliers = [
  { id: supplierA, code: 'SUP-PLATES', name: 'Preview Plates' },
  { id: supplierB, code: 'SUP-MOTORS', name: 'Preview Motors' },
];
const terms = {
  plan_id: plan, proposal_id: proposal, currency: 'EUR', status: 'draft', revision: 4, total: '105.14',
  lines: [
    { ...draftLines[0], supplier: suppliers[0], unit_price: '2.07', line_total: '24.84' },
    { ...draftLines[1], supplier: suppliers[1], unit_price: '8.03', line_total: '80.30' },
  ],
};
const approval = {
  plan_id: plan, proposal_id: proposal, terms_revision: 4, currency: 'EUR', total: '105.14',
  policy_id: policy, policy_revision: 7, state: 'approved',
  stages: [{ key: 'department', state: 'approved' }, { key: 'budget', state: 'approved' }],
};
const batch = {
  batch_id: batchId, plan_id: plan, proposal_id: proposal, order_id: order, line_id: line,
  terms_revision: 4, policy_id: policy, policy_revision: 7,
  state: 'issued', currency: 'EUR', total: '105.14', issued_at: '2026-10-01T12:00:00Z',
  purchase_orders: terms.lines.map(({ supplier, ...row }, i) => ({
    purchase_order_id: [poA, poB][i], supplier, total: row.line_total,
    lines: [{ purchase_line_id: [poLineA, poLineB][i], ...row }],
  })),
};
const basePermissions = ['sales.order.read', 'inventory.reservation.read', 'inventory.material-plan.read'];
const readProposal = 'procurement.proposal.read';
const readBatch = 'procurement.purchase-order.read';
const issueBatch = 'procurement.purchase-order.issue';
const editor = ['master-data.partner.read', 'procurement.proposal.terms.write'];
const withoutRead = [...basePermissions, readProposal, issueBatch];

async function fixture(page: Page) {
  const calls: { method: string; path: string; org: string; token: string; body?: unknown }[] = [];
  const state = {
    approvals: approval,
    approvalsDown: false,
    termsDown: false,
    batch: null as typeof batch | null,
    getBatch: undefined as ((route: Route, org: string) => Promise<void>) | undefined,
    postBatch: (route: Route) => route.fulfill({ status: 201, json: batch }),
  };
  await page.route('**/demo-oidc.json', route => route.fulfill({ status: 404 }));
  await page.route('**/v1/**', async route => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    const { authorization, 'x-erp4us-organization': org, 'x-erp4us-tenant': requestTenant } = request.headers();
    const token = authorization?.replace(/^Bearer /, '') ?? '';
    if (path === '/v1/session') {
      const permissions = token === 'reader' ? [...basePermissions, readProposal, readBatch]
        : token === 'without-read' ? withoutRead : [...basePermissions, readProposal, readBatch, issueBatch, ...editor];
      return route.fulfill({ json: { actor_id: uuid(40), grants: [
        { tenant_id: tenant, organization_id: organization, permissions },
        { tenant_id: tenant, organization_id: otherOrg, permissions: withoutRead },
      ] } });
    }
    const method = request.method();
    if (path.startsWith('/v1/inventory/') || path.startsWith('/v1/procurement/') || path === '/v1/partners') {
      expect(requestTenant).toBe(tenant);
      expect([organization, otherOrg]).toContain(org);
    }
    calls.push({ method, path, org, token, body: request.postDataJSON() ?? undefined });
    if (method === 'POST') {
      expect(path).toBe(purchasePath);
      expect(org).toBe(organization);
      expect(token).not.toBe('reader');
      expect(token).not.toBe('without-read');
      return state.postBatch(route);
    }
    if (path === purchasePath) {
      expect(token).not.toBe('without-read');
      expect(org).toBe(organization);
      if (state.getBatch) return state.getBatch(route, org);
      return state.batch ? route.fulfill({ json: state.batch }) : route.fulfill({ status: 404 });
    }
    if (path === reservation) return route.fulfill({ json: {
      order_id: order, line_id: line, sku: 'NM-1000', product_revision: 'A', unit: 'EA',
      requested: '10', reserved: '0', outstanding: '10', warehouse_id: uuid(31), source_event_id: uuid(32),
    } });
    if (path === `${reservation}/material-plan`) return route.fulfill({ json: {
      id: plan, production: [], materials: draftLines.map(row => ({
        ...row, requested: row.pieces, reserved: '0', uncovered: row.pieces,
      })),
    } });
    if (path === `${reservation}/material-plan/procurement-handoff`) {
      return route.fulfill({ json: { plan_id: plan, event_id: uuid(33), state: 'published' } });
    }
    if (path === proposalPath) return route.fulfill({ json: {
      proposal_id: proposal, plan_id: plan, order_id: order, line_id: line, status: 'draft',
      bom: { sku: 'NM-1000', revision: 'A' }, lines: draftLines,
    } });
    if (path === `${proposalPath}/terms`) {
      return route.fulfill(state.termsDown ? { status: 503 } : { json: terms });
    }
    if (path === `${proposalPath}/approval`) {
      return route.fulfill(state.approvalsDown ? { status: 503 } : { json: state.approvals });
    }
    if (path === '/v1/partners') return route.fulfill({ json: { items: suppliers, next_after: null } });
    return route.fulfill({ status: 404 });
  });
  return { state, calls, posts: () => calls.filter(call => call.method === 'POST').map(call => call.body) };
}

async function signInOnReservation(page: Page, token: string) {
  await page.goto(pagePath);
  await page.getByRole('banner').getByRole('link', { name: 'Local demo access' }).click();
  await page.getByLabel('Demo token').fill(token);
  const verified = page.waitForResponse(response => response.url().endsWith('/v1/session'));
  await page.getByRole('button', { name: 'Use for this session' }).click();
  expect((await verified).ok()).toBe(true);
  await page.goBack();
}

const review = (page: Page) => page.getByRole('button', { name: 'Review purchase issuance', exact: true });
const confirm = (page: Page) => page.getByRole('button', { name: 'Issue local purchase orders', exact: true });
const check = (page: Page) => page.getByRole('button', { name: 'Check for issued purchase orders', exact: true });
const issued = (page: Page) => page.getByRole('region', { name: 'Issued purchase orders', exact: true });

test('issuance requires all stages, matching terms and both purchase grants', async ({ page }) => {
  const api = await fixture(page);
  api.state.approvals = {
    ...approval, state: 'pending', stages: [approval.stages[0], { key: 'budget', state: 'pending' }],
  };
  await signInOnReservation(page, 'issuer');
  await expect(page.getByText('Budget approval · Pending', { exact: true })).toBeVisible();
  await expect(check(page)).toBeEnabled();
  await expect(review(page)).toHaveCount(0);

  api.state.approvals = { ...approval, terms_revision: 3 };
  await page.getByRole('button', { name: 'Refresh approvals' }).click();
  await expect(page.getByText('The terms or approval policy changed.', { exact: false })).toBeVisible();
  await expect(review(page)).toHaveCount(0);

  api.state.approvals = approval;
  await page.getByRole('button', { name: 'Refresh approvals' }).click();
  await expect(review(page)).toBeVisible();
  await signInOnReservation(page, 'reader');
  await expect(page.getByText('All required stages are approved.', { exact: false })).toBeVisible();
  await expect(check(page)).toBeEnabled();
  await expect(review(page)).toHaveCount(0);

  await signInOnReservation(page, 'without-read');
  await expect(page.getByText('All required stages are approved.', { exact: false })).toBeVisible();
  await expect(check(page)).toHaveCount(0);
  await expect(review(page)).toHaveCount(0);
  expect(api.posts()).toEqual([]);
});

test('explicit local issuance reconciles an uncertain result and retries the same command', async ({ page }) => {
  const api = await fixture(page);
  await signInOnReservation(page, 'issuer');
  await expect(page.getByRole('button', { name: 'Save draft purchase terms' })).toBeVisible();
  await review(page).press('Enter');
  await expect(page.getByRole('region', { name: 'Confirm purchase issuance' })).toContainText('EUR 105.14');
  await page.getByRole('button', { name: 'Cancel purchase issuance review' }).click();
  await expect(confirm(page)).toHaveCount(0);
  expect(api.posts()).toEqual([]);

  api.state.postBatch = route => route.abort('connectionreset');
  await review(page).click();
  await confirm(page).click();
  await expect(page.getByText('The issuance outcome is unknown.', { exact: false })).toBeVisible();
  await expect(confirm(page)).toHaveCount(0);
  expect(api.posts()).toEqual([{
    command_id: expect.stringMatching(/^[0-9a-f-]{36}$/), terms_revision: 4, policy_id: policy, policy_revision: 7,
  }]);

  await check(page).click();
  await expect(page.getByText('Retrying these terms reuses the original command.', { exact: false })).toBeVisible();
  await page.getByRole('main').getByRole('link', { name: 'Sales orders', exact: true }).click();
  await page.goBack();
  await expect(page.getByText('Retrying these terms reuses the original command.', { exact: false })).toBeVisible();
  // The original commit completes after reconciliation saw no batch. The POST
  // returns the idempotent 200 and must receive the exact original command.
  api.state.batch = batch;
  api.state.postBatch = route => route.fulfill({ status: 200, json: api.state.batch });
  await page.getByRole('button', { name: 'Review purchase issuance retry', exact: true }).click();
  expect(api.posts()).toHaveLength(1);
  await page.getByRole('button', { name: 'Retry the same purchase issuance', exact: true }).click();
  await expect(issued(page)).toBeVisible();
  expect(api.posts()).toHaveLength(2);
  expect(api.posts()[0]).toEqual(api.posts()[1]);
  await expect(issued(page)).toContainText(`Issuance batch ID: ${batchId}`);
  await expect(issued(page)).toContainText('Issued batch total: EUR 105.14');
  await expect(issued(page)).toContainText('Nothing is sent to suppliers.');
  const plates = issued(page).getByRole('table', { name: 'SUP-PLATES · Preview Plates' });
  await expect(plates.getByRole('row', { name: /NM-ENDPLATE/ }).getByRole('cell')).toHaveText(['12 EA', 'EUR 2.07', 'EUR 24.84']);
  const motors = issued(page).getByRole('table', { name: 'SUP-MOTORS · Preview Motors' });
  await expect(motors.getByRole('row', { name: /NM-MOTOR · A/ }).getByRole('cell')).toHaveText(['10 EA', 'EUR 8.03', 'EUR 80.30']);
  await expect(issued(page).getByRole('table')).toHaveCount(2);
  await expect(page.getByRole('button', { name: 'Save draft purchase terms' })).toHaveCount(0);
  await expect(review(page)).toHaveCount(0);
});

test('a read-only actor reopens the persisted record after reload without approval or terms', async ({ page }) => {
  const api = await fixture(page);
  await signInOnReservation(page, 'issuer');
  api.state.postBatch = route => {
    api.state.batch = batch;
    return route.abort('connectionreset');
  };
  await review(page).click();
  await confirm(page).click();
  await expect(page.getByText('The issuance outcome is unknown.', { exact: false })).toBeVisible();

  api.state.termsDown = true;
  api.state.approvalsDown = true;
  await check(page).click();
  await expect(issued(page)).toContainText(batchId);
  await signInOnReservation(page, 'reader');
  await expect(issued(page)).toContainText(batchId);
  await expect(issued(page).getByRole('table')).toHaveCount(2);
  await expect(review(page)).toHaveCount(0);
  expect(api.posts()).toHaveLength(1);
  expect(api.calls.filter(call => call.path === purchasePath && call.token === 'reader').map(call => call.method)).toEqual(['GET']);
});

test('an expired credential removes an already-open confirmation', async ({ page }) => {
  await page.clock.install();
  const api = await fixture(page);
  const exp = Math.floor(await page.evaluate(() => Date.now()) / 1000) + 120;
  const token = `fixture.${Buffer.from(JSON.stringify({ exp })).toString('base64url')}.placeholder`;
  await signInOnReservation(page, token);
  await review(page).click();
  await expect(confirm(page)).toBeVisible();
  await page.clock.fastForward(121_000);
  await expect(confirm(page)).toHaveCount(0);
  await expect(review(page)).toHaveCount(0);
  await expect(page.getByText('Purchase order access was denied or your session expired.')).toBeVisible();
  expect(api.posts()).toEqual([]);
});

test('scope switching prevents a delayed issued record from appearing in another scope', async ({ page }) => {
  const api = await fixture(page);
  let release: (() => void) | undefined;
  api.state.getBatch = async route => {
    await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({ json: batch }).catch(() => {}); // The browser may already have aborted this request.
  };
  await signInOnReservation(page, 'issuer');
  await expect.poll(() => !!release).toBe(true);
  await page.getByLabel('Company and organization').selectOption({ index: 1 });
  await expect(page.getByText('All required stages are approved.', { exact: false })).toBeVisible();
  await expect(check(page)).toHaveCount(0);
  release?.();
  await expect(issued(page)).toHaveCount(0);
  await expect(review(page)).toHaveCount(0);
  expect(api.posts()).toEqual([]);
});
