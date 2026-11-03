// Browser smoke test of the calls example (docs/calls.md, "Browser smoke test"): two
// Chromium contexts with fake media devices call each other through the example page.
// The harness (crates/push-smoke/tests/browser_calls.rs) starts the stack and serve.mjs and
// passes, in the environment:
//   CALLS_BASE_URL   the page's origin (serve.mjs)
//   CALLS_A_TOKEN, CALLS_B_TOKEN   access tokens of two accepted contacts
//   CALLS_A_USER, CALLS_B_USER     their user ids
//   CALLS_RELAY      "1": relay-only ICE (iceTransportPolicy "relay") through TURN

import { createRequire } from "node:module";
import { expect, test } from "@playwright/test";

const require = createRequire(import.meta.url);
const protobufFile = require.resolve("protobufjs/dist/protobuf.min.js");
const env = (name) => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set: run through the push-smoke harness (docs/calls.md)`);
  return value;
};
const relay = process.env.CALLS_RELAY === "1";

test.describe.configure({ mode: "serial" });

async function openClient(browser, token) {
  const context = await browser.newContext({ permissions: ["microphone"] });
  // protobuf.js from the local install (the same file the page pins with SRI), so the
  // test does not depend on the CDN.
  await context.route("https://cdn.jsdelivr.net/npm/protobufjs@*/dist/protobuf.min.js", (route) =>
    route.fulfill({
      path: protobufFile,
      headers: { "content-type": "text/javascript", "access-control-allow-origin": "*" },
    }));
  const page = await context.newPage();
  page.on("console", (message) => {
    if (message.type() === "error") console.log(`[page] ${message.text()}`);
  });
  await page.goto(env("CALLS_BASE_URL"));
  await page.fill("#token", token);
  if (relay) await page.check("#relay");
  await page.click("#connect");
  await expect(page.locator("#status")).toHaveText("ready");
  return { context, page };
}

// The bytes of audio received on the call's peer connection, and the type of the local
// candidate of the selected pair.
function media(page) {
  return page.evaluate(async () => {
    const call = [...window.callClient.calls.values()][0];
    if (!call || !call.pc) return { bytes: 0, local: null, state: "none" };
    const stats = await call.pc.getStats();
    let bytes = 0;
    let pairId = null;
    stats.forEach((report) => {
      if (report.type === "inbound-rtp" && report.kind === "audio") bytes += report.bytesReceived;
      if (report.type === "transport" && report.selectedCandidatePairId) pairId = report.selectedCandidatePairId;
    });
    const pair = pairId ? stats.get(pairId) : null;
    const local = pair ? stats.get(pair.localCandidateId)?.candidateType : null;
    return { bytes, local, state: call.pc.connectionState };
  });
}

async function expectAudioFlowing(page) {
  const first = await media(page);
  await expect.poll(async () => (await media(page)).bytes, { timeout: 20_000 }).toBeGreaterThan(first.bytes + 1000);
  const now = await media(page);
  expect(now.state).toBe("connected");
  if (relay) expect(now.local).toBe("relay");
  return now;
}

function logOf(page) {
  return page.locator("#log");
}

let a;
let b;

test.beforeAll(async ({ browser }) => {
  a = await openClient(browser, env("CALLS_A_TOKEN"));
  b = await openClient(browser, env("CALLS_B_TOKEN"));
});

test.beforeEach(async () => {
  for (const side of [a, b]) await side.page.evaluate(() => { document.getElementById("log").textContent = ""; });
});

test.afterAll(async () => {
  for (const side of [a, b]) {
    if (!side) continue;
    // A failed test can leave a call up: hang it up so the next run starts clean.
    await side.page.evaluate(() => {
      for (const id of window.callClient.calls.keys()) window.callClient.hangup(id);
    }).catch(() => {});
    await side.page.waitForTimeout(500);
    await side.context.close();
  }
});

async function placeAndAnswer() {
  await a.page.fill("#peer", env("CALLS_B_USER"));
  await a.page.click("#call");
  const incoming = b.page.locator('.call[data-status="incoming"]');
  await expect(incoming).toHaveCount(1);
  await incoming.locator("button.answer").click();
  await expect(a.page.locator('.call[data-status="active"]')).toHaveCount(1);
  await expect(b.page.locator('.call[data-status="active"]')).toHaveCount(1);
}

async function hangUpAndExpectCompleted() {
  await a.page.locator(".call button.hang").click();
  await expect(a.page.locator(".call")).toHaveCount(0);
  await expect(b.page.locator(".call")).toHaveCount(0);
  await expect(logOf(a.page)).toContainText("ended: COMPLETED");
  await expect(logOf(b.page)).toContainText("ended: COMPLETED");
}

test("a call connects, carries audio both ways and ends completed", async () => {
  await placeAndAnswer();
  await expectAudioFlowing(a.page);
  await expectAudioFlowing(b.page);
  await hangUpAndExpectCompleted();
});

test("a callee reconnect resumes the call through CallSync and an ICE restart", async () => {
  await placeAndAnswer();
  await expectAudioFlowing(b.page);
  // Drop the callee's WebSocket (not the media): the client reconnects, reads CallSync
  // and restarts ICE through CallRenegotiate.
  await b.page.evaluate(() => window.callClient.ws.close(4999));
  await expect(logOf(b.page)).toContainText("CallSync: 1 call(s)", { timeout: 20_000 });
  await expect(logOf(b.page)).toContainText("ICE restart");
  await expect(b.page.locator("#status")).toHaveText("ready");
  await expectAudioFlowing(b.page);
  await expectAudioFlowing(a.page);
  await hangUpAndExpectCompleted();
});

test("a declined call ends declined on both sides", async () => {
  await a.page.fill("#peer", env("CALLS_B_USER"));
  await a.page.click("#call");
  const incoming = b.page.locator('.call[data-status="incoming"]');
  await expect(incoming).toHaveCount(1);
  await incoming.locator("button.decline").click();
  await expect(a.page.locator(".call")).toHaveCount(0);
  await expect(b.page.locator(".call")).toHaveCount(0);
  await expect(logOf(a.page)).toContainText("ended: DECLINED");
  await expect(logOf(b.page)).toContainText("ended: DECLINED");
});
