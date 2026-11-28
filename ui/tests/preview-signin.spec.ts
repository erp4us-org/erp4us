import { test, expect, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';

test.use({ ignoreHTTPSErrors: true }); // Only for the locally generated preview certificate.

async function login(page: Page, user: 'production' | 'auditor' | 'platform-monitor' | 'department-approver' | 'budget-approver' | 'purchase-issuer') {
  const passwords: Record<string, string> = JSON.parse(readFileSync('../.erp4us-preview/credentials.json', 'utf8'));
  await page.goto('/local-access');
  await page.getByRole('button', { name: 'Sign in with Keycloak' }).click();
  await page.waitForURL('https://127.0.0.1:9443/**');
  await page.locator('#username').fill(user);
  await page.locator('#password').fill(passwords[user]);
  await page.locator('#kc-login').click();
  if (user === 'platform-monitor') {
    await expect(page.getByRole('banner')).toContainText('Platform operations', { timeout: 20000 });
  } else {
    await expect(page.getByLabel('Company and organization')).toBeVisible({ timeout: 20000 });
  }
  await expect(page).toHaveURL('http://127.0.0.1:4200/');
}

test('real OIDC: production planner calculates the NovaMotion bill of materials', async ({ page }) => {
  test.skip(process.env.ERP4US_REAL_OIDC_E2E !== '1', 'Requires the dedicated Keycloak preview stack');
  await login(page, 'production');
  await page.getByRole('link', { name: 'Manufacturing' }).click();
  await page.getByRole('button', { name: 'Calculate gross needs' }).click();
  await expect(page.getByRole('row').filter({ hasText: 'NM-ENDPLATE' })).toContainText('20 EA');
  await page.getByRole('link', { name: 'Master data', exact: true }).click();
  await page.getByRole('link', { name: 'Business partners' }).click();
  await page.getByLabel('Role', { exact: true }).selectOption('customer');
  await expect(page.getByRole('row').filter({ hasText: 'Atlas Handling Technologies GmbH' })).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: 'VectorDrive' })).toHaveCount(0);
  await page.getByLabel('Role', { exact: true }).selectOption('supplier');
  await expect(page.getByRole('row').filter({ hasText: 'VectorDrive' })).toBeVisible();
});

test('real OIDC: the other tenant cannot read NovaMotion manufacturing data', async ({ page }) => {
  test.skip(process.env.ERP4US_REAL_OIDC_E2E !== '1', 'Requires the dedicated Keycloak preview stack');
  // Read the actual access token from the session-call request in memory. Send
  // it back to the owning service with explicitly unrelated scope headers.
  const session = page.waitForRequest(request => request.url().endsWith('/v1/session'));
  await login(page, 'auditor');
  const authorization = (await (await session).allHeaders()).authorization;
  expect(await page.getByRole('link', { name: 'Manufacturing' }).count()).toBe(0);
  expect(await page.getByRole('link', { name: 'Sales orders' }).count()).toBe(0);
  const result = await page.request.get('/v1/manufacturing/products', { headers: {
    Authorization: authorization,
    'X-Erp4us-Tenant': 'ddcae139-c1e8-47d2-8b48-349cb6f30ce5',
    'X-Erp4us-Organization': '3e793fb5-75a7-45f6-8f66-45a58fcec674',
  }});
  expect(result.status()).toBe(403);
  expect((await page.request.get('/v1/admin/application-health', { headers: { Authorization: authorization } })).status()).toBe(403);
});

test('real OIDC: platform monitor sees actual application health', async ({ page }) => {
  test.skip(process.env.ERP4US_REAL_OIDC_E2E !== '1', 'Requires the dedicated Keycloak preview stack');
  await login(page, 'platform-monitor');
  await page.getByRole('link', { name: 'Administration' }).click();
  await expect(page.getByRole('heading', { name: 'Application health' })).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: 'master-data' }).getByRole('cell', { name: 'Ready', exact: true })).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: 'manufacturing' }).getByRole('cell', { name: 'Ready', exact: true })).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: 'jetstream' }).getByRole('cell', { name: 'Ready', exact: true })).toBeVisible();
  await expect(page.getByText('Stored messages', { exact: true })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Sales orders to Inventory' })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Material plans to Procurement' })).toBeVisible();
  await expect(page.getByText('Awaiting acknowledgement', { exact: true })).toHaveCount(3);
  await expect(page.getByRole('heading', { name: 'Article Relay heartbeat' })).toBeVisible();
  await expect(page.getByText('Heartbeat received recently', { exact: true })).toBeVisible({ timeout: 20000 });
  await expect(page.getByText('Seconds since last receipt', { exact: true })).toBeVisible();
});

test('real OIDC: production user saves and confirms a priced order; an authorized issuer records its approved purchase', async ({ page, browser }) => {
  test.skip(process.env.ERP4US_REAL_OIDC_E2E !== '1', 'Requires the dedicated Keycloak preview stack');
  const issuance = process.env.ERP4US_PURCHASE_ISSUANCE_E2E === '1';
  if (process.env.ERP4US_BUDGET_APPROVAL_E2E === '1') test.setTimeout(issuance ? 100000 : 70000);
  await login(page, 'production');
  await page.getByRole('link', { name: 'Sales orders' }).click();
  await expect(page.getByRole('option', { name: /CUS-ATLAS/ })).toHaveCount(1);
  await page.getByLabel('Customer', { exact: true }).selectOption({ label: 'CUS-ATLAS · Atlas Handling Technologies GmbH' });
  await expect(page.getByRole('option', { name: /NM-1000/ })).toHaveCount(1);
  await page.getByLabel('Pieces', { exact: true }).fill('10');
  const saved = page.waitForResponse(response => response.url().endsWith('/v1/sales/orders')
    && response.request().method() === 'POST');
  await page.getByRole('button', { name: 'Save draft' }).click();
  const draft = await saved;
  expect(draft.status()).toBe(201);
  const {id} = await draft.json();
  await expect(page.getByText('Draft saved: EUR 24900.00')).toBeVisible();
  await page.getByRole('link', { name: 'Manufacturing' }).click();
  await page.getByRole('link', { name: 'Sales orders' }).click();
  const match = page.getByRole('row').filter({ hasText: id }).filter({ hasText: 'Atlas Handling Technologies GmbH' })
    .filter({ hasText: 'NM-1000' }).filter({ hasText: 'EUR 24900.00' });
  await expect(match).toContainText('10 EA');
  await match.getByRole('button', { name: 'Review confirmation' }).click();
  await expect(page.getByText('Confirmation commits the saved commercial terms.', { exact: false })).toBeVisible();
  const committed = page.waitForResponse(response => response.url().endsWith(`/v1/sales/orders/${id}/confirmation`));
  await page.getByRole('button', { name: 'Confirm order' }).click();
  expect((await committed).status()).toBe(200);
  await expect(match.getByRole('cell', { name: 'Confirmed', exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Manufacturing' }).click();
  await page.getByRole('link', { name: 'Sales orders' }).click();
  await expect(match.getByRole('cell', { name: 'Confirmed', exact: true })).toBeVisible();
  await match.getByRole('link', { name: 'Check reservation' }).click();
  await expect(page.getByRole('heading', { name: 'NM-1000 · A' })).toBeVisible();
  await expect(page.getByText('0 EA', { exact: true })).toBeVisible();
  await expect(page.getByText('10 EA', { exact: true })).toHaveCount(2);
  await page.getByRole('button', { name: 'Plan and reserve materials' }).click();
  const materials = page.getByRole('table', { name: 'Material requirements' });
  for (const [sku, pieces] of Object.entries({
    'NM-ENDPLATE': 12, 'NM-MOTOR': 6, 'NM-SPINDLE': 10, 'NM-RAIL': 8, 'NM-SENSOR': 10,
  })) {
    await expect(materials.getByRole('row').filter({ hasText: sku }).getByRole('cell').last()).toHaveText(`${pieces} EA`);
  }
  const production = page.getByRole('table', { name: 'Production requirements' });
  for (const sku of ['NM-1000', 'NM-1100', 'NM-1200']) {
    await expect(production.getByRole('row').filter({ hasText: sku }).getByRole('cell').last()).toHaveText('10 EA');
  }
  await page.getByRole('button', { name: 'Send shortages to Procurement' }).click();
  const proposal = page.getByRole('table', { name: 'Purchase proposal draft' });
  await expect(proposal.getByRole('row')).toHaveCount(6);
  for (const [sku, pieces] of Object.entries({
    'NM-ENDPLATE': 12, 'NM-MOTOR': 6, 'NM-SPINDLE': 10, 'NM-RAIL': 8, 'NM-SENSOR': 10,
  })) {
    await expect(proposal.getByRole('row').filter({ hasText: sku }).getByRole('cell').last()).toHaveText(`${pieces} EA`);
  }
  const proposalRef = page.getByText(/^Proposal ID:/);
  await expect(proposalRef).toBeVisible();
  const ref = await proposalRef.innerText();
  if (process.env.ERP4US_PURCHASE_TERMS_E2E === '1') {
    for (const [sku, price] of Object.entries({
      'NM-ENDPLATE': '30.10', 'NM-MOTOR': '149.95', 'NM-SPINDLE': '45.20',
      'NM-RAIL': '12.00', 'NM-SENSOR': '7.35',
    })) {
      await page.getByRole('combobox', { name: `Supplier ${sku}` })
        .selectOption({ label: 'SUP-VECTORDRIVE · VectorDrive Components GmbH' });
      await page.getByRole('textbox', { name: `Unit price (EUR) ${sku}` }).fill(price);
    }
    await page.getByRole('button', { name: 'Save draft purchase terms' }).click();
    await expect(page.getByText(/Draft total: EUR 1882\.40/)).toBeVisible();
  }
  await page.getByRole('main').getByRole('link', { name: 'Sales orders', exact: true }).click();
  await match.getByRole('link', { name: 'Check reservation' }).click();
  await expect(proposalRef).toHaveText(ref);
  if (process.env.ERP4US_PURCHASE_TERMS_E2E === '1') {
    await expect(page.getByText(/Draft total: EUR 1882\.40/)).toBeVisible();
    await expect(page.getByRole('button', { name: 'Save draft purchase terms' })).toBeDisabled();
  }
  if (process.env.ERP4US_BUDGET_APPROVAL_E2E === '1') {
    await expect(page.getByRole('button', { name: /Approve stage/ })).toHaveCount(0);
    for (const [user, stage] of [
      ['department-approver', 'Department approval'], ['budget-approver', 'Budget approval'],
    ] as const) {
      const context = await browser.newContext({ ignoreHTTPSErrors: true });
      try {
        const approver = await context.newPage();
        await login(approver, user);
        await approver.getByRole('link', { name: 'Sales orders' }).click();
        await approver.getByRole('row').filter({hasText:id}).getByRole('link', { name:'Check reservation' }).click();
        await expect(approver.getByText(/Reviewed draft total: EUR 1882\.40/)).toBeVisible();
        await expect(approver.getByRole('button', { name:'Save draft purchase terms' })).toHaveCount(0);
        await approver.getByRole('button', {name:`Approve stage: ${stage}`}).click();
        await expect(approver.getByText(`${stage} · Approved`, {exact:true})).toBeVisible();
        if (user === 'department-approver') {
          await expect(approver.getByText('Budget approval · Pending', {exact:true})).toBeVisible();
        }
      } finally {
        await context.close();
      }
    }
    await page.getByRole('button', {name:'Refresh approvals'}).click();
    await expect(page.getByText('All required stages are approved. Approval alone does not issue purchase orders.')).toBeVisible();
  }
  await expect(page.getByRole('button', { name: /place|approve/i })).toHaveCount(0);
  if (issuance) {
    const batchPath = /\/v1\/procurement\/proposals\/by-plan\/[^/]+\/purchase-orders$/;
    const reviewName = 'Review purchase issuance';
    const confirmationName = 'Issue local purchase orders';
    const heading = 'Issued purchase orders';
    await expect(page.getByRole('button', { name: reviewName })).toHaveCount(0);
    const context = await browser.newContext({ ignoreHTTPSErrors: true });
    let purchaseOrderId: string;
    try {
      const issuer = await context.newPage();
      await login(issuer, 'purchase-issuer');
      await issuer.getByRole('link', { name: 'Sales orders' }).click();
      const unissued = issuer.waitForResponse(response => batchPath.test(response.url()) && response.request().method() === 'GET');
      await issuer.getByRole('row').filter({ hasText: id }).getByRole('link', { name: 'Check reservation' }).click();
      expect((await unissued).status()).toBe(404); // Neither approval issued anything.
      await expect(issuer.getByRole('button', { name: 'Save draft purchase terms' })).toHaveCount(0);
      await expect(issuer.getByRole('button', { name: /Approve stage/ })).toHaveCount(0);
      await expect(issuer.getByRole('button', { name: confirmationName })).toHaveCount(0);
      await issuer.getByRole('button', { name: reviewName }).click();
      const recorded = issuer.waitForResponse(response => batchPath.test(response.url()) && response.request().method() === 'POST');
      await issuer.getByRole('button', { name: confirmationName }).click();
      const response = await recorded;
      expect(response.status()).toBe(201);
      const result = await response.json();
      expect(result).toMatchObject({ order_id: id, state: 'issued', total: '1882.40', currency: 'EUR' });
      expect(result.purchase_orders).toHaveLength(1);
      expect(result.purchase_orders[0].lines).toHaveLength(5);
      expect(result.purchase_orders[0].lines).toContainEqual(expect.objectContaining({ sku: 'NM-ENDPLATE', pieces: '12' }));
      purchaseOrderId = result.purchase_orders[0].purchase_order_id;
      await expect(issuer.getByRole('heading', { name: heading })).toBeVisible();
      await expect(issuer.getByText(new RegExp(purchaseOrderId))).toBeVisible();
    } finally {
      await context.close();
    }
    await page.getByRole('main').getByRole('link', { name: 'Sales orders', exact: true }).click();
    const retrieved = page.waitForResponse(response => batchPath.test(response.url()) && response.request().method() === 'GET');
    await match.getByRole('link', { name: 'Check reservation' }).click();
    const response = await retrieved;
    expect(response.status()).toBe(200);
    expect((await response.json()).purchase_orders[0].purchase_order_id).toBe(purchaseOrderId);
    await expect(page.getByRole('heading', { name: heading })).toBeVisible();
    const issued = page.getByRole('table', { name: 'SUP-VECTORDRIVE · VectorDrive Components GmbH' });
    await expect(issued.getByRole('row').filter({ hasText: 'NM-ENDPLATE' })).toContainText('12 EA');
    await expect(page.getByRole('button', { name: reviewName })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Save draft purchase terms' })).toHaveCount(0);
  }
});
