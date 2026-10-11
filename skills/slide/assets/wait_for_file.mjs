#!/usr/bin/env node

import { stat } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";

const [filePath] = process.argv.slice(2);
const timeoutMs = Number.parseInt(
  process.env.SLIDEV_START_TIMEOUT_MS ?? "30000",
  10,
);

if (!filePath) {
  throw new Error("usage: wait_for_file.mjs <file>");
}
if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
  throw new Error("SLIDEV_START_TIMEOUT_MS must be a positive integer");
}

const deadline = Date.now() + timeoutMs;
while (Date.now() < deadline) {
  try {
    if ((await stat(filePath)).size > 0) {
      process.exit(0);
    }
  } catch (error) {
    if (error?.code !== "ENOENT") {
      throw error;
    }
  }
  await delay(100);
}

throw new Error(`file did not become ready within ${timeoutMs} ms: ${filePath}`);
