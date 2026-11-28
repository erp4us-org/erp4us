import { expect, test, type Locator, type Page, type Route } from '@playwright/test';

const id = (n: number) => `b0000000-0000-4000-8000-${n.toString(16).padStart(12, '0')}`;
const [tenant, org, otherOrg, order, line, plan, purchase, plate, motor, otherLine] =
  Array.from({length: 10}, (_, index) => id(index + 1));
const reservation = (salesLine: string) => `/v1/inventory/reservations/${order}/${salesLine}`;
const path = `/sales/orders/${order}/reservations/${line}`;
const sourcePath = `/v1/procurement/proposals/by-plan/${plan}/purchase-orders/receipt-source`;
const receiptPath = `${reservation(line)}/material-plan/purchase-orders/${purchase}/lines/${plate}/receipts`;
const source = 'procurement.purchase-order.receipt-source.read';
const read = 'inventory.purchase-receipt.read';
const post = 'inventory.purchase-receipt.post';
const permissions = {
  buyer: ['procurement.purchase-order.read', read, post],
  discovery: [source],
  reader: [source, read],
  poster: [source, post],
  receiver: [source, read, post],
};
type Actor = keyof typeof permissions;
type Intent = {receipt_id: string; pieces: string};
const issuedPlate = {purchase_order_id: purchase, purchase_line_id: plate, sku: 'NM-ENDPLATE', revision: null, unit: 'EA', ordered_pieces: '12'};
const issuedMotor = {...issuedPlate, purchase_line_id: motor, sku: 'NM-MOTOR', revision: 'A', ordered_pieces: '10'};

function allocation(sequence: 1 | 2) {
  const {ordered_pieces: _, ...identity} = issuedPlate;
  return {
    ...identity, plan_id: plan, through_receipt_sequence: sequence,
    receipt_allocated_pieces: sequence === 1 ? '6' : '12',
    line_open_pieces: sequence === 1 ? '6' : '0',
    plan_held_pieces: sequence === 1 ? '14' : '20',
    plan_remaining_shortage_pieces: sequence === 1 ? '6' : '0',
  };
}

const noStore = {'Cache-Control': 'no-store'};
const posted = (intent: Intent, sequence: 1 | 2) => ({
  ...intent, posted_at: '2026-11-20T12:00:00Z', allocation_after: allocation(sequence),
});

async function fixture(page: Page) {
  const calls: {method: string; endpoint: string; scope: string}[] = [];
  const posts: Intent[] = [];
  const state = {
    current: null as ReturnType<typeof allocation> | null,
    getSource: (route: Route) => route.fulfill({headers: noStore, json: {plan_id: plan, lines: [issuedPlate, issuedMotor]}}),
    getCurrent: (route: Route): Promise<void> => state.current
      ? route.fulfill({headers: noStore, json: state.current}) : route.fulfill({status: 404}),
    post: (route: Route, intent: Intent): Promise<void> => {
      if (intent.pieces !== '6') return route.fulfill({status: 409, json: {code: 'over_receipt'}});
      const sequence = state.current ? 2 : 1;
      state.current = allocation(sequence);
      return route.fulfill({status: 201, headers: noStore, json: posted(intent, sequence)});
    },
  };
  await page.route('**/demo-oidc.json', route => route.fulfill({status: 404}));
  await page.route('**/v1/**', async route => {
    const request = route.request();
    const endpoint = new URL(request.url()).pathname;
    const headers = request.headers();
    const credential = headers['authorization']?.slice(7) ?? '';
    const actor = (credential.includes('.') ? 'receiver' : credential) as Actor;
    const base = ['inventory.reservation.read', 'inventory.material-plan.read', 'sales.order.read'];
    if (endpoint === '/v1/session') return route.fulfill({json: {actor_id: id(50), grants: [
      {tenant_id: tenant, organization_id: org, permissions: [...base, ...permissions[actor]]},
      {tenant_id: tenant, organization_id: otherOrg, permissions: [...base, read, post]},
    ]}});
    const method = request.method(), scope = headers['x-erp4us-organization'];
    calls.push({method, endpoint, scope});
    if (endpoint === sourcePath || endpoint === receiptPath) {
      expect(headers['x-erp4us-tenant']).toBe(tenant);
      expect(scope).toBe(org);
      expect(actor).not.toBe('buyer');
      if (endpoint === sourcePath) {
        expect(permissions[actor]).toContain(source);
        expect(method).toBe('GET');
        return state.getSource(route);
      }
      if (method === 'GET') {
        expect(permissions[actor]).toContain(read);
        return state.getCurrent(route);
      }
      expect(method).toBe('POST');
      expect(permissions[actor]).toContain(post);
      const intent = request.postDataJSON() as Intent;
      expect(intent).toEqual({receipt_id: expect.stringMatching(/^[0-9a-f-]{36}$/), pieces: expect.any(String)});
      posts.push(intent);
      return state.post(route, intent);
    }
    for (const salesLine of [line, otherLine]) {
      if (endpoint === reservation(salesLine)) return route.fulfill({json: {
        order_id: order, line_id: salesLine, sku: 'NM-1000', product_revision: 'A', unit: 'EA',
        requested: '10', reserved: '0', outstanding: '10', warehouse_id: id(31), source_event_id: id(32),
      }});
      if (endpoint === `${reservation(salesLine)}/material-plan`) return route.fulfill({json: {
        id: salesLine === line ? plan : id(11), order_id: order, line_id: salesLine, production: [],
        materials: [{sku: 'NM-ENDPLATE', revision: null, requested: '20', reserved: '8', uncovered: '12'}],
      }});
    }
    return route.fulfill({status: 404});
  });
  return {state, calls, posts};
}

async function signIn(page: Page, token: string) {
  await page.goto(path);
  await page.getByRole('banner').getByRole('link', {name: 'Local demo access'}).click();
  await page.getByLabel('Demo token').fill(token);
  const verified = page.waitForResponse(response => response.url().endsWith('/v1/session'));
  await page.getByRole('button', {name: 'Use for this session'}).click();
  expect((await verified).ok()).toBe(true);
  await page.goBack();
  await expect(original(page)).toBeVisible();
}

const original = (page: Page) => page.getByRole('row', {name: /NM-ENDPLATE/});
const panel = (page: Page) => page.getByRole('region', {name: 'Local purchase receiving', exact: true});
const action = (page: Page, name: string) => panel(page).getByRole('button', {name, exact: true});
const historical = (page: Page) => panel(page).getByRole('region', {name: 'Original receipt snapshot'});
const current = (page: Page) => panel(page).getByRole('region', {name: 'Current Inventory allocation'});
const refresh = (page: Page) => action(page, 'Refresh current allocation');
const quantity = (page: Page) => panel(page).getByLabel('Physical pieces received (EA)');
const measure = (region: Locator, label: string) => region.getByText(`${label}:`, {exact: true}).locator('+ dd');
const discover = (page: Page) => action(page, 'Find issued purchase lines');
const review = (page: Page) => action(page, 'Review receipt');
const confirm = (page: Page) => action(page, 'Confirm and post receipt');

async function choosePlate(page: Page) {
  await discover(page).click();
  await panel(page).getByLabel('Issued purchase line').selectOption({label: '1. NM-ENDPLATE — 12 EA'});
}

test('only the exact scoped source grant discovers lines; Inventory read and post are independent', async ({page}) => {
  const api = await fixture(page);
  await signIn(page, 'buyer');
  await expect(panel(page)).toHaveCount(0);
  expect(api.calls.filter(call => call.endpoint === sourcePath || call.endpoint === receiptPath)).toEqual([]);

  await signIn(page, 'discovery');
  await choosePlate(page);
  await expect(panel(page)).toContainText('Ordered quantity: 12 EA');
  await expect(refresh(page)).toHaveCount(0);
  await expect(review(page)).toHaveCount(0);

  await signIn(page, 'reader');
  await choosePlate(page);
  await refresh(page).click();
  await expect(current(page)).toContainText('No posted receipt allocation was observed for this line.');
  await expect(review(page)).toHaveCount(0);

  await signIn(page, 'poster');
  await choosePlate(page);
  await expect(review(page)).toBeVisible();
  await expect(refresh(page)).toHaveCount(0);
  await page.getByLabel('Company and organization').selectOption({index: 1});
  await expect(panel(page)).toHaveCount(0);
  expect(api.calls.filter(call => call.endpoint === receiptPath)).toEqual([{method: 'GET', endpoint: receiptPath, scope: org}]);
});

test('redacted discovery and two explicit partial receipts leave the original plan unchanged', async ({page}) => {
  const api = await fixture(page);
  // Even if a response over-shares, only validated narrow receiving fields can reach the UI.
  api.state.getSource = route => route.fulfill({headers: noStore, json: {plan_id: plan, lines: [
    {...issuedPlate, supplier: {name: 'PRIVATE SUPPLIER'}, unit_price: '843.26'}, issuedMotor,
  ]}});
  await signIn(page, 'receiver');
  await choosePlate(page);
  await expect(panel(page)).toContainText('Ordered quantity: 12 EA');
  await expect(panel(page)).not.toContainText(/EUR|unit price|PRIVATE SUPPLIER|843\.26/i);
  expect(api.calls.filter(call => call.endpoint.startsWith('/v1/procurement/')).map(call => call.endpoint)).toEqual([sourcePath]);
  await refresh(page).click();
  await expect(current(page)).toContainText('No posted receipt allocation was observed for this line.');
  await expect(current(page)).not.toContainText(/Open on purchase line:|Held for plan row:/);

  await quantity(page).fill('06');
  await expect(review(page)).toBeDisabled();
  await quantity(page).fill('6');
  await review(page).press('Enter');
  const confirmation = panel(page).getByRole('region', {name: 'Confirm physical receipt'});
  await expect(confirmation).toContainText('NM-ENDPLATE');
  await expect(confirmation).toContainText('6 EA');
  await expect(confirm(page)).toBeFocused();
  expect(api.posts).toEqual([]);
  await confirm(page).press('Enter');
  await expect(measure(historical(page), 'Open on purchase line')).toHaveText('6 EA');
  await expect(measure(historical(page), 'Held for plan row')).toHaveText('14 EA');
  await refresh(page).click();
  await expect(measure(current(page), 'Open on purchase line')).toHaveText('6 EA');

  await quantity(page).fill('7');
  await review(page).click();
  await confirm(page).click();
  await expect(panel(page)).toContainText('That quantity exceeds what is still open on the purchase line.');
  await expect(measure(historical(page), 'Held for plan row')).toHaveText('14 EA');

  await quantity(page).fill('6');
  await review(page).click();
  await confirm(page).click();
  await expect(measure(historical(page), 'Open on purchase line')).toHaveText('0 EA');
  await refresh(page).click();
  await expect(measure(current(page), 'Held for plan row')).toHaveText('20 EA');
  await expect(measure(current(page), 'Remaining plan shortage')).toHaveText('0 EA');
  await expect(original(page).getByRole('cell')).toHaveText(['20 EA', '8 EA', '12 EA']);
  expect(api.posts.map(value => value.pieces)).toEqual(['6', '7', '6']);
  expect(new Set(api.posts.map(value => value.receipt_id)).size).toBe(3);
});

test('an uncertain physical lot stays locked across reopen; a newer allocation is not its confirmation', async ({page}) => {
  const api = await fixture(page);
  api.state.post = route => route.fulfill({status: 503, body: 'PRIVATE-TIMEOUT'});
  await signIn(page, 'receiver');
  await choosePlate(page);
  await quantity(page).fill('6');
  await review(page).click();
  await confirm(page).click();
  await expect(panel(page)).toContainText('Inventory has not confirmed the outcome of this lot.');
  await expect(panel(page)).not.toContainText('PRIVATE-TIMEOUT');
  await expect(quantity(page)).toHaveCount(0);
  await expect(historical(page)).toHaveCount(0);
  expect(api.posts).toHaveLength(1);
  await refresh(page).click();
  await expect(current(page)).toContainText('No posted receipt allocation was observed for this line.');

  await page.getByRole('main').getByRole('link', {name: 'Sales orders', exact: true}).click();
  await page.goBack();
  await expect(panel(page)).toContainText('Inventory has not confirmed the outcome of this lot.');
  api.state.current = allocation(2);
  await refresh(page).click();
  await expect(measure(current(page), 'Open on purchase line')).toHaveText('0 EA');
  await expect(measure(current(page), 'Held for plan row')).toHaveText('20 EA');
  await expect(historical(page)).toHaveCount(0);
  expect(api.posts).toHaveLength(1);

  // Replay needs Inventory authorization but does not consult Procurement; it can
  // return the original commit even though another receipt has since allocated.
  api.state.getSource = route => route.fulfill({status: 503});
  api.state.post = (route, intent) => route.fulfill({status: 200, headers: noStore, json: posted({...intent, receipt_id: id(98)}, 1)});
  await action(page, 'Retry the same receipt').press('Enter');
  await expect(panel(page)).toContainText('The receipt outcome could not be verified.');
  await expect(historical(page)).toHaveCount(0);
  await expect(quantity(page)).toHaveCount(0);

  api.state.post = (route, intent) => route.fulfill({status: 200, headers: noStore, json: posted(intent, 1)});
  await action(page, 'Retry the same receipt').press('Enter');
  await expect(measure(historical(page), 'Open on purchase line')).toHaveText('6 EA');
  await expect(measure(historical(page), 'Held for plan row')).toHaveText('14 EA');
  expect(api.posts).toEqual([api.posts[0], api.posts[0], api.posts[0]]);
  expect(api.calls.filter(call => call.endpoint === sourcePath)).toHaveLength(1);
  await refresh(page).click();
  await expect(measure(current(page), 'Held for plan row')).toHaveText('20 EA');
  await expect(measure(historical(page), 'Held for plan row')).toHaveText('14 EA');
  await expect(original(page).getByRole('cell')).toHaveText(['20 EA', '8 EA', '12 EA']);
});

test('scope changes abort discovery; a denied response clears an already selected line', async ({page}) => {
  const api = await fixture(page);
  let release: (() => void) | undefined;
  const normal = api.state.getSource;
  api.state.getSource = async route => {
    await new Promise<void>(resolve => { release = resolve; });
    await normal(route).catch(() => {}); // Context may already have aborted the request.
  };
  await signIn(page, 'receiver');
  await discover(page).click();
  await expect.poll(() => !!release).toBe(true);
  const aborted = page.waitForEvent('requestfailed', {predicate: request => request.url().endsWith(sourcePath)});
  await page.getByLabel('Company and organization').selectOption({index: 1});
  await expect(panel(page)).toHaveCount(0);
  await aborted;
  release?.();
  await page.getByLabel('Company and organization').selectOption({index: 0});
  api.state.getSource = normal;
  await choosePlate(page);
  api.state.getCurrent = route => route.fulfill({status: 403, body: 'PRIVATE-DENIAL'});
  await refresh(page).click();
  await expect(panel(page)).toContainText('Receiving access was denied or your session expired.');
  await expect(panel(page)).not.toContainText(/NM-ENDPLATE|PRIVATE-DENIAL/);
  await expect(review(page)).toHaveCount(0);
  expect(api.posts).toEqual([]);
});

test('source bounds and owner conflicts fail closed without posting another ID', async ({page}) => {
  const api = await fixture(page);
  await signIn(page, 'receiver');
  const normal = api.state.getSource;
  api.state.getSource = route => route.fulfill({headers: noStore, json: {
    plan_id: plan, lines: Array.from({length: 33}, (_, i) => ({...issuedPlate, purchase_line_id: id(100 + i)})),
  }});
  await discover(page).click();
  await expect(panel(page)).toContainText('The issued receiving lines could not be verified.');
  await expect(panel(page).getByLabel('Issued purchase line')).toHaveCount(0);

  api.state.getSource = route => route.fulfill({headers: noStore, json: {
    plan_id: plan, lines: [issuedPlate], unexpected: 'X'.repeat(16 * 1024),
  }});
  const oversized = page.waitForResponse(response => response.url().endsWith('/receipt-source'));
  await discover(page).click();
  await oversized;
  await expect(panel(page)).toContainText('The issued receiving lines could not be verified.');
  await expect(panel(page).getByLabel('Issued purchase line')).toHaveCount(0);

  api.state.getSource = normal;
  await choosePlate(page);
  await quantity(page).fill('6');
  await review(page).click();
  api.state.post = route => route.fulfill({status: 409, json: {code: 'plan_ineligible'}});
  await confirm(page).click();
  await expect(panel(page)).toContainText('The linked plan row cannot receive this lot in its current state.');

  // An ID binding mismatch cannot be safely repaired by choosing another ID.
  await review(page).click();
  api.state.post = route => route.fulfill({status: 409, json: {code: 'receipt_intent_changed'}});
  await confirm(page).click();
  await expect(panel(page)).toContainText('This receipt ID does not match the original line or quantity.');
  await expect(action(page, 'Retry the same receipt')).toHaveCount(0);
  await expect(quantity(page)).toHaveCount(0);
  await page.getByRole('main').getByRole('link', {name: 'Sales orders', exact: true}).click();
  await page.goBack();
  await expect(panel(page)).toContainText('This receipt ID does not match the original line or quantity.');
  await expect(quantity(page)).toHaveCount(0);
  expect(api.posts).toHaveLength(2);
});

test('an expired credential removes a ready confirmation and cannot post', async ({page}) => {
  await page.clock.install();
  const api = await fixture(page);
  const exp = Math.floor(await page.evaluate(() => Date.now()) / 1000) + 120;
  await signIn(page, `fixture.${Buffer.from(JSON.stringify({exp})).toString('base64url')}.placeholder`);
  await choosePlate(page);
  await quantity(page).fill('6');
  await review(page).click();
  await expect(confirm(page)).toBeVisible();
  await page.clock.fastForward(121_000);
  await expect(panel(page)).toHaveCount(0);
  expect(api.posts).toEqual([]);
});

test('a route line change aborts the old allocation request and clears its selection', async ({page}) => {
  const api = await fixture(page);
  let release: (() => void) | undefined;
  api.state.getCurrent = async route => {
    await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({headers: noStore, json: allocation(2)}).catch(() => {});
  };
  await signIn(page, 'receiver');
  await choosePlate(page);
  await refresh(page).click();
  await expect.poll(() => !!release).toBe(true);
  const aborted = page.waitForEvent('requestfailed', {predicate: request => request.url().endsWith(receiptPath)});
  // Navigate via Angular Router so the reservation component can be reused.
  await page.evaluate(({order, otherLine}) => {
    history.pushState(null, '', `/sales/orders/${order}/reservations/${otherLine}`);
    dispatchEvent(new PopStateEvent('popstate'));
  }, {order, otherLine});
  await aborted;
  release?.();
  await expect(discover(page)).toBeVisible();
  await expect(current(page)).toHaveCount(0);
  await expect(historical(page)).toHaveCount(0);
  expect(api.posts).toEqual([]);
});
