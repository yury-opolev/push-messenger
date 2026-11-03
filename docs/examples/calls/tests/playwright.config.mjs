// Playwright configuration of the calls example's browser smoke test. Chromium with fake
// media devices: a synthetic microphone, and no permission prompt.
import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "*.spec.mjs",
  timeout: 90_000,
  expect: { timeout: 15_000 },
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  use: {
    browserName: "chromium",
    headless: true,
    launchOptions: {
      args: [
        "--use-fake-device-for-media-stream",
        "--use-fake-ui-for-media-stream",
        "--autoplay-policy=no-user-gesture-required",
      ],
    },
  },
});
