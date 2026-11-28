import { defineConfig } from '@playwright/test';

const requestedPort = process.env.ERP4US_PLAYWRIGHT_FIXTURE_PORT;
const fixturePort = process.env.ERP4US_REAL_OIDC_E2E !== '1' && requestedPort &&
  /^[1-9]\d{3,4}$/.test(requestedPort) && Number(requestedPort) <= 65535 ? requestedPort : undefined;
const url = `http://127.0.0.1:${fixturePort || '4200'}`;

export default defineConfig({
  testDir: './tests',
  use: { baseURL: url, locale: 'en-US' },
  webServer: {
    command: fixturePort ? `npm start -- --port ${fixturePort}` : 'npm start',
    url,
    reuseExistingServer: !process.env.CI
  }
});
