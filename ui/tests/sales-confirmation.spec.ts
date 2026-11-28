import { test, expect } from '@playwright/test';
import { execFileSync } from 'node:child_process';

test('fixture: review and confirm a saved order in the authorized scope', async ({ page }) => {
  test.skip(process.env.ERP4US_SALES_E2E !== '1', 'Requires the scoped Sales fixture stack');
  const token = execFileSync('python3', ['tests/integration/identity_fixture.py', 'token'], {
    cwd: '..', encoding: 'utf8',
  }).trim();
  await page.goto('/local-access');
  await page.getByLabel('Demo token').fill(token);
  await page.getByRole('button', { name: 'Use for this session' }).click();
  await page.getByRole('link', { name: 'Sales orders' }).click();
  await expect(page.getByRole('option', { name: /CUS-ATLAS/ })).toHaveCount(1);
  await page.getByLabel('Customer', { exact: true }).selectOption({ label: 'CUS-ATLAS · Atlas Handling Technologies GmbH' });
  await expect(page.getByRole('option', { name: /NM-1000/ })).toHaveCount(1);
  await page.getByLabel('Pieces', { exact: true }).fill('10');
  const saved = page.waitForResponse(response => response.url().endsWith('/v1/sales/orders')
    && response.request().method() === 'POST');
  await page.getByRole('button', { name: 'Save draft' }).click();
  const response = await saved;
  expect(response.status()).toBe(201);
  const {id} = await response.json();
  const row = page.getByRole('row').filter({hasText: id});
  await expect(row.getByRole('cell', {name: 'Draft', exact: true})).toBeVisible();
  await row.getByRole('button', {name: 'Review confirmation'}).click();
  await page.getByRole('button', {name: 'Cancel review'}).click();
  await expect(row.getByRole('cell', {name: 'Draft', exact: true})).toBeVisible();
  await row.getByRole('button', {name: 'Review confirmation'}).click();
  await page.getByRole('button', {name: 'Confirm order'}).click();
  await expect(row.getByRole('cell', {name: 'Confirmed', exact: true})).toBeVisible();
  await expect(row).toContainText('EUR 24900.00');
  await row.getByRole('link', {name: 'Check reservation'}).click();
  await expect(page.getByRole('heading', {name: 'NM-1000 · A'})).toBeVisible();
  await expect(page.getByText('0 EA', {exact: true})).toBeVisible();
  await expect(page.getByText('10 EA', {exact: true})).toHaveCount(2);
  const committedPlan = page.waitForResponse(response => response.url().endsWith('/material-plan')
    && response.request().method() === 'POST');
  await page.getByRole('button', {name: 'Plan and reserve materials'}).click();
  const result = await committedPlan;
  expect(result.status()).toBe(201);
  const data: {materials: {sku: string; uncovered: string}[]} = await result.json();
  const endPlates = data.materials.find(item => item.sku === 'NM-ENDPLATE');
  const rowForEndPlates = page.getByRole('row').filter({hasText: 'NM-ENDPLATE'});
  await expect(rowForEndPlates.getByRole('cell', {name: '20 EA', exact: true}).first()).toBeVisible();
  await expect(rowForEndPlates.getByRole('cell').last()).toHaveText(`${endPlates?.uncovered} EA`);
  if (process.env.ERP4US_PROCUREMENT_E2E === '1') {
    await page.getByRole('button', {name: 'Send shortages to Procurement'}).click();
    const proposal = page.getByRole('table', {name: 'Purchase proposal draft'});
    await expect(proposal.getByRole('row').filter({hasText: 'NM-ENDPLATE'}).getByRole('cell'))
      .toHaveText(`${endPlates?.uncovered} EA`);
    const proposalRef = page.getByText(/^Proposal ID:/);
    const stableRef = await proposalRef.innerText();
    const unitPrice = process.env.ERP4US_BUDGET_APPROVAL_E2E === '1' ? 100 : 2;
    if (process.env.ERP4US_PURCHASE_TERMS_E2E === '1') {
      const shortages = data.materials.filter(item => item.uncovered !== '0');
      for (const {sku} of shortages) {
        await page.getByRole('combobox', {name:`Supplier ${sku}`})
          .selectOption({label: 'SUP-VECTORDRIVE · VectorDrive Components GmbH'});
        await page.getByRole('textbox', {name:`Unit price (EUR) ${sku}`}).fill(`${unitPrice}.00`);
      }
      await page.getByRole('button', {name:'Save draft purchase terms'}).click();
      const total = unitPrice * shortages.reduce((sum, item) => sum + Number(item.uncovered), 0);
      await expect(page.getByText(`Revision: 1 · Draft total: EUR ${total}.00`)).toBeVisible();
      await expect(page.getByRole('button', {name:'Save draft purchase terms'})).toBeDisabled();
    }
    await page.getByRole('main').getByRole('link', {name:'Sales orders', exact:true}).click();
    await row.getByRole('link', {name:'Check reservation'}).click();
    await expect(proposalRef).toHaveText(stableRef);
    if (process.env.ERP4US_PURCHASE_TERMS_E2E === '1') {
      const total = unitPrice * data.materials.reduce((sum, item) => sum + Number(item.uncovered), 0);
      await expect(page.getByText(`Revision: 1 · Draft total: EUR ${total}.00`)).toBeVisible();
      if (process.env.ERP4US_BUDGET_APPROVAL_E2E === '1') {
        const budget = page.getByRole('region', {name:'Budget approval'});
        await expect(budget.getByText('Department approval · Pending', {exact:true})).toBeVisible();
        await expect(budget.getByText('Budget approval · Pending', {exact:true})).toBeVisible();
        await expect(budget.getByRole('button', {name:/Approve stage/})).toHaveCount(0);
      }
    }
  }
  await page.getByLabel('Company and organization').selectOption({index: 2});
  await expect(page.getByRole('heading', {name: 'NM-1000 · A'})).toHaveCount(0);
  await expect(rowForEndPlates).toHaveCount(0);
  await expect(page.getByRole('heading', {name:'Draft purchase terms'})).toHaveCount(0);
});
