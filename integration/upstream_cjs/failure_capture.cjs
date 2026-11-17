// Loaded with `node --require` ahead of a preserved browser test (run_browser_tests.py); the test file is not changed.
//
// HARS_FAILURE_SHOTS=DIR: once an assertion has failed or a Playwright call has timed out, closing the browser (every
// test closes it in `finally`, right after the failure) first saves a screenshot of each open page in DIR.
// Failures are noticed where the error is built, through an accessor on the error class's prototype that its
// constructor assigns through. Nothing wraps a test's calls, so stacks, assert's generated messages and the timing of
// a passing test stay as they are; only closing the browser after a failure does more.
// HARS_TRACE_DIR=DIR: every browser context records a Playwright trace, saved in DIR when it closes. That changes
// timing, so the runner sets it only for the collection rerun of a test already judged failed.
'use strict';
const Module = require('module'), fs = require('fs'), path = require('path');
const SHOTS = process.env.HARS_FAILURE_SHOTS, TRACES = process.env.HARS_TRACE_DIR;
let failure = null, saved = 0;

// `property` is assigned in the constructor; the setter turns it back into the own data property the plain
// assignment would have made, so the error looks the same to everything that reads or prints it.
function noticeBuilt(errorClass, property, fallback) {
  Object.defineProperty(errorClass.prototype, property, {
    configurable: true,
    get() { return fallback; },
    set(value) {
      Object.defineProperty(this, property, { value, writable: true, enumerable: true, configurable: true });
      if (!failure) failure = `${errorClass.name}: ${String(this.message).split('\n')[0].slice(0, 300)}`;
    },
  });
}
noticeBuilt(require('assert').AssertionError, 'generatedMessage', undefined);

let traceCount = 0;
const tracing = new Set();
async function startTrace(context) {
  if (tracing.has(context)) return;
  await context.tracing.start({ screenshots: true, snapshots: true });
  tracing.add(context);
}
async function stopTrace(context) {
  if (!tracing.delete(context)) return;
  fs.mkdirSync(TRACES, { recursive: true });
  await context.tracing.stop({ path: path.join(TRACES, `trace-${++traceCount}.zip`) }).catch(() => {});
}

async function saveShots(browser) {
  fs.mkdirSync(SHOTS, { recursive: true });
  const lines = [`failure: ${failure}`];
  for (const [i, page] of browser.contexts().flatMap(c => c.pages()).entries()) {
    const file = `page-${i + 1}.png`;
    try {
      await page.screenshot({ path: path.join(SHOTS, file), timeout: 5000 });
      saved++;
      lines.push(`${file} ${page.url()}`);
    } catch (error) {
      lines.push(`${file} not saved: ${String(error.message).split('\n')[0]}`);
    }
  }
  fs.writeFileSync(path.join(SHOTS, 'failure.txt'), lines.join('\n') + '\n');
}

function watchBrowser(browser) {
  const close = browser.close.bind(browser);
  if (TRACES) {
    const newContext = browser.newContext.bind(browser), newPage = browser.newPage.bind(browser);
    browser.newContext = async (...args) => {
      const context = await newContext(...args), closeContext = context.close.bind(context);
      await startTrace(context);
      context.close = async (...a) => { await stopTrace(context); return closeContext(...a); };
      return context;
    };
    browser.newPage = async (...args) => {
      const page = await newPage(...args), context = page.context(), closePage = page.close.bind(page);
      await startTrace(context);
      page.close = async (...a) => { await stopTrace(context); return closePage(...a); };
      return page;
    };
  }
  browser.close = async (...args) => {
    if (failure && SHOTS) await saveShots(browser).catch(() => {});
    for (const context of [...tracing]) await stopTrace(context);
    return close(...args);
  };
  return browser;
}

function watchPlaywright(playwright) {
  if (playwright.__harsWatched) return;
  playwright.__harsWatched = true;
  noticeBuilt(playwright.errors.TimeoutError, 'name', 'Error');
  for (const type of [playwright.chromium, playwright.firefox, playwright.webkit]) {
    const launch = type.launch.bind(type);
    type.launch = async (...args) => watchBrowser(await launch(...args));
  }
}

const load = Module._load;
Module._load = function (request) {
  const loaded = load.apply(this, arguments);
  if (request === 'playwright') watchPlaywright(loaded);
  return loaded;
};

if (SHOTS) process.on('exit', () => {
  if (!failure) return;
  try {
    fs.mkdirSync(SHOTS, { recursive: true });
    fs.appendFileSync(path.join(SHOTS, 'failure.txt'), `screenshots saved: ${saved}\n`);
  } catch {}
});
