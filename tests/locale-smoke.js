#!/usr/bin/env node
// Manual smoke test for the bundled Forgejo's pages: no page-level error banner at
// the default locale, at zh-CN, and with a browser whose navigator.language is the
// unusable tag `C` that made webcomponents.js throw on every page.
//
// It is not run by `cargo test` because it needs Playwright's Chromium; it is the
// committed form of the live proof. Run it with:
//
//   GEWU_URL=http://127.0.0.1:3000 GEWU_ADMIN_USER=owner \
//   GEWU_ADMIN_PASSWORD=... node tests/locale-smoke.js
//
// The password is read from the environment and is never printed.
const { chromium } = require('playwright');

const url = (process.env.GEWU_URL || 'http://127.0.0.1:3000').replace(/\/$/, '');
const user = process.env.GEWU_ADMIN_USER || 'owner';
const password = process.env.GEWU_ADMIN_PASSWORD;
const issue = process.env.GEWU_ISSUE || '/team/workspace/issues/1';
const pull = process.env.GEWU_PULL || '/team/workspace/pulls/1';
if (!password) {
  console.error('GEWU_ADMIN_PASSWORD is not set');
  process.exit(2);
}

const pages = ['/', issue, pull];
const HOSTILE = "Object.defineProperty(Navigator.prototype,'language',{get(){return 'C'}});" +
  "Object.defineProperty(Navigator.prototype,'languages',{get(){return ['C']}});";

(async () => {
  const browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-gpu'] });
  let failed = false;
  for (const [label, options, init] of [
    ['default', {}, null],
    ['zh-CN', { locale: 'zh-CN' }, null],
    ['C-forced', {}, HOSTILE],
  ]) {
    const context = await browser.newContext(options);
    if (init) await context.addInitScript(init);
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message.slice(0, 160)));
    page.on('console', (message) => {
      if (message.type() === 'error' && !message.text().includes('favicon')) {
        errors.push(message.text().slice(0, 160));
      }
    });
    await page.goto(`${url}/user/login`, { waitUntil: 'domcontentloaded' });
    await page.fill('input[name="user_name"]', user);
    await page.fill('input[name="password"]', password);
    await page.press('input[name="password"]', 'Enter');
    await page.waitForTimeout(1200);
    for (const path of pages) {
      await page.goto(url + path, { waitUntil: 'domcontentloaded' });
      await page.waitForTimeout(400);
      const body = await page.innerText('body');
      const banner = (body.match(/JavaScript error[^\n]*/) || [])[0] || null;
      const nav = await page.evaluate(() => navigator.language);
      const lang = await page.evaluate(() => document.documentElement.lang);
      if (banner) {
        failed = true;
        console.error(JSON.stringify({ label, path, nav, lang, banner }));
      } else {
        console.log(JSON.stringify({ label, path, nav, lang, banner: null, errors }));
      }
    }
    await context.close();
  }
  await browser.close();
  if (failed) process.exit(1);
})().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
