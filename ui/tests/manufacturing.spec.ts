import { test, expect } from '@playwright/test';
import { execFileSync } from 'node:child_process';

test('authorized NovaMotion planner explores two persisted manufacturing structures', async ({ page }) => {
  test.skip(process.env.ERP4US_MANUFACTURING_E2E !== '1', 'Requires the Manufacturing Compose overlay');
  const accessToken = execFileSync('python3', ['tests/integration/identity_fixture.py', 'token'], {
    cwd: '..', encoding: 'utf8',
  }).trim();
  await page.goto('/manufacturing');
  await expect(page.getByRole('link', { name: 'Manufacturing' })).toHaveCount(0);
  await page.getByRole('banner').getByRole('link', { name: 'Local demo access' }).click();
  await page.getByLabel('Demo token').fill(accessToken);
  await page.getByRole('button', { name: 'Use for this session' }).click();
  await page.getByRole('link', { name: 'Manufacturing' }).click();
  await expect(page.getByLabel('Company and organization')).toHaveValue('0');
  await expect(page.getByLabel('Product and revision').locator('option')).toHaveCount(2);
  await page.getByLabel('Pieces').fill('10');
  await page.getByRole('button', { name: 'Calculate gross needs' }).click();
  await expect(page.getByText('10 × NM-1100', { exact: false }).first()).toBeVisible();
  await expect(page.getByText('10 × NM-1200', { exact: false }).first()).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: 'NM-ENDPLATE' })).toContainText('20 EA');
  await expect(page.getByRole('row').filter({ hasText: 'NM-SCREW' })).toContainText('120 EA');
  await page.getByLabel('Product and revision').selectOption({ index: 1 });
  await page.getByRole('button', { name: 'Calculate gross needs' }).click();
  await expect(page.getByRole('heading', { name: 'Bill of materials: NM-2000 · A × 10' })).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: 'NM-MOTOR' })).toHaveCount(0);
  await page.getByLabel('Company and organization').selectOption({ index: 2 });
  await expect(page.getByText('Sign in with permission', { exact: false })).toBeVisible();
  await expect(page.getByRole('table')).toHaveCount(0);
});
