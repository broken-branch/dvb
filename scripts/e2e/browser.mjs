// The app window's acceptance step (check 5): open the desk's
// launch URL, open the task page, and wait until it says "Done" with no reload.
//
//   node scripts/e2e/browser.mjs <launch-url> <task-id> [timeout-ms]
//
// Needs playwright-core next to it; CI installs the exact ui/package.json
// version into scripts/e2e. Uses the runner's Chrome (channel "chrome"),
// or CHROME_PATH (e.g. /usr/bin/chromium).
import { chromium } from 'playwright-core';

const [url, task, timeout = '120000'] = process.argv.slice(2);
if (!url || !task) {
  console.error('usage: browser.mjs <launch-url> <task-id> [timeout-ms]');
  process.exit(2);
}
const launch = process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : { channel: 'chrome' };
const browser = await chromium.launch(launch);
try {
  const page = await browser.newPage();
  await page.goto(url);
  await page.goto(new URL(`/#task/${task}`, url).href);
  await page.locator('[data-task-state]').waitFor({ timeout: 30000 });
  console.log('browser: ready on the task page');
  let reloads = 0;
  page.on('load', () => { reloads += 1; });
  await page.locator('[data-task-state]', { hasText: 'Done' }).waitFor({ timeout: Number(timeout) });
  if (reloads > 0) throw new Error(`the page reloaded ${reloads} times`);
  console.log(`browser: the task page shows Done without a reload`);
} finally {
  await browser.close();
}
