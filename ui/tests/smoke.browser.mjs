import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { chromium } from 'playwright-core';
import { createServer } from 'vite';

const server = await createServer({
  server: { port: 0, host: '127.0.0.1' },
  plugins: [
    {
      name: 'smoke-events',
      configureServer(server) {
        server.middlewares.use((request, response, next) => {
          if (!request.url?.startsWith('/events?')) return next();
          response.writeHead(200, {
            'content-type': 'text/event-stream',
            'dvb-version': responses['/version'].version,
          });
          // EOF clears the live source's readiness, so keep this stream open.
          response.write(': live\n\n');
        });
      },
    },
  ],
});
const { responses, mutationResponses } = await server.ssrLoadModule(
  '/tests/support/engine-fixtures.ts',
);
const fixtures = { ...responses, ...mutationResponses };
await server.listen();
const browser = await chromium.launch({
  channel: 'chrome',
  headless: true,
  args: ['--no-sandbox'],
});
try {
  const page = await browser.newPage({
    reducedMotion: 'reduce',
    permissions: ['local-network-access'],
  });
  // Chrome's automatic icon request is outside the app's asset graph.
  await page.route('**/favicon.ico', (route) => route.fulfill({ status: 204 }));
  await page.route(
    (url) => url.pathname.startsWith('/api/'),
    (route) => {
      const pathname = new URL(route.request().url()).pathname.replace(/^\/api/, '');
      const method = route.request().method();
      const key = method === 'GET' ? pathname : `${method} ${pathname}`;
      assert.ok(Object.hasOwn(fixtures, key), `Missing engine fixture: ${key}`);
      const value = fixtures[key];
      return route.fulfill({
        ...(value === undefined ? { status: 204 } : { json: value }),
        headers: { 'dvb-version': responses['/version'].version },
      });
    },
  );
  await page.route('**/desk/events', (route) =>
    route.fulfill({ contentType: 'text/event-stream', body: ': live\n\n' }),
  );
  await page.route('**/github/prefill', (route) =>
    route.fulfill({ json: { organization: null, owner_name: null, owner_email: null } }),
  );
  await page.route('**/theme.css', (route) => route.fulfill({ status: 204 }));
  await page.route('**/', async (route) => {
    const response = await route.fetch();
    const html = (await response.text()).replace(
      '<head>',
      `<head><meta name="dvb-version" content="${responses['/version'].version}">`,
    );
    await route.fulfill({ response, body: html });
  });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(message.text());
  });
  const base = `http://127.0.0.1:${server.httpServer.address().port}/`;
  const routes = [
    'home',
    'projects',
    'decision/local',
    'project/harbour',
    'project/harbour/brief',
    'project/harbour/decisions',
    'project/harbour/conversation',
    'project/harbour/activity',
    'project/harbour/settings',
    'project/new',
    'task/resume/changes',
    'task/resume/review',
    'task/resume/checks',
    'task/resume/decisions',
    'task/resume/feed',
    'company',
    'company/project/harbour/role/harbour-backend',
    'usage',
    'settings/appearance',
    'settings/accessibility',
    'settings/preferences',
    'settings/models',
    'settings/updates',
    'setup',
  ];
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    for (const route of routes) {
      await page.goto(`${base}#${route}`);
      await page.locator('.page').waitFor();
      await page.locator('.page :is(h1, h2)').first().waitFor();
      if (route.includes('/role/')) await page.getByRole('dialog').waitFor();
      await page.evaluate(() => document.fonts.ready);
      assert.equal(
        await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),
        false,
        `${route} at ${width}: horizontal overflow`,
      );
      if (process.env.DVB_DEMO_DIR) {
        await mkdir(process.env.DVB_DEMO_DIR, { recursive: true });
        await page.screenshot({
          path: `${process.env.DVB_DEMO_DIR}/${route.replaceAll('/', '-')}-${width}.png`,
          fullPage: true,
        });
      }
      if (route === 'company') {
        await page.getByRole('tab', { name: 'Cards', exact: true }).click();
        await page.locator('.role-card').first().waitFor();
        assert.equal(
          await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),
          false,
          `company/cards at ${width}: horizontal overflow`,
        );
      }
    }
    // Load the embedded machine, sign-in, GitHub and walkthrough screens without creating work.
    // With an engine connected, setup opens on Sign in; Back reaches the Machine step.
    for (const screen of ['machine', 'sign-in', 'github', 'walkthrough', 'first-project']) {
      if (screen === 'machine') {
        await page.getByRole('button', { name: 'Back', exact: true }).click();
      } else if (screen === 'first-project') {
        for (let step = 0; step < 3; step++) {
          await page.getByRole('button', { name: 'Next', exact: true }).click();
          assert.equal(
            await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),
            false,
            `walkthrough ${step + 2} at ${width}: horizontal overflow`,
          );
        }
        await page.getByRole('button', { name: 'Done', exact: true }).click();
      } else {
        await page.getByRole('button', { name: /^(Continue|Skip for now)$/ }).click();
      }
      await page.locator('.setup-body :is(h2, h3)').first().waitFor();
      assert.equal(
        await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),
        false,
        `${screen} at ${width}: horizontal overflow`,
      );
    }
  }
  if (process.env.DVB_DEMO_DIR) {
    // The README screenshot: a demo project with real model ids, at README framing.
    const { demoResponses } = await server.ssrLoadModule('/tests/support/demo-fixtures.ts');
    Object.assign(fixtures, demoResponses);
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(`${base}#project/catalogue`);
    await page.reload();
    await page.locator('.kanban-column').first().waitFor();
    await page.evaluate(() => document.fonts.ready);
    await page.screenshot({ path: `${process.env.DVB_DEMO_DIR}/cockpit.png` });
  }
  assert.deepEqual(errors, []);
  console.log(
    `Browser smoke passed: ${routes.length} routes, Company cards and setup screens at desktop and mobile widths.`,
  );
} finally {
  await browser.close();
  await server.close();
}
