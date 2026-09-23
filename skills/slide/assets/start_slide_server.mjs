#!/usr/bin/env node

import { spawn } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import { get as httpGet } from "node:http";
import { createRequire } from "node:module";
import { createServer } from "node:net";
import { basename, dirname, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const [slideFile, urlFile] = process.argv.slice(2);
const LOOPBACK_ADDRESS = "127.0.0.1";

if (!slideFile || !urlFile) {
  throw new Error("usage: start_slide_server.mjs <slide-file> <url-file>");
}

const runtimeManifestPath = resolve("package.json");
const runtimeRequire = createRequire(runtimeManifestPath);
const matter = runtimeRequire("gray-matter");
const slidePath = resolve(slideFile);
const slideSource = await readFile(slidePath, "utf8");
const { data: frontmatter } = matter(slideSource);
const theme = frontmatter.theme;
if (typeof theme !== "string" || theme.trim() === "") {
  throw new Error("Slidev preview requires a non-empty frontmatter theme");
}

const runtimeManifest = JSON.parse(await readFile(runtimeManifestPath, "utf8"));
const dependencies = {
  ...runtimeManifest.dependencies,
  ...runtimeManifest.devDependencies,
};
const themePackage = [
  theme,
  `@slidev/theme-${theme}`,
  `slidev-theme-${theme}`,
].find((candidate) => Object.hasOwn(dependencies, candidate));
if (!themePackage) {
  throw new Error(`Slidev theme package is not declared: ${theme}`);
}
runtimeRequire.resolve(`${themePackage}/package.json`);

const port = await reserveEphemeralPort();
const url = `http://${LOOPBACK_ADDRESS}:${port}/1?embedded=true`;
const slideRoot = dirname(slidePath);
const viteConfigPath = resolve(slideRoot, "vite.config.ts");
const helperRoot = dirname(fileURLToPath(import.meta.url));
const viteConfig = await readFile(resolve(helperRoot, "vite.config.ts"), "utf8");
await writeFile(viteConfigPath, viteConfig, "utf8");
const viteCacheDir = resolve(
  slideRoot,
  ".slidev-vite-cache",
  basename(slidePath),
);

const slidevCli = resolve("node_modules/@slidev/cli/bin/slidev.mjs");
const child = spawn(
  process.execPath,
  [
    slidevCli,
    slidePath,
    "--theme",
    theme,
    "--port",
    String(port),
    "--remote",
    String(port),
    "--bind",
    LOOPBACK_ADDRESS,
  ],
  {
    env: {
      ...process.env,
      CI: "1",
      SLIDEV_CACHE_DIR: viteCacheDir,
    },
    stdio: ["ignore", "pipe", "pipe"],
  },
);

forwardOutput(child.stdout, process.stdout);
forwardOutput(child.stderr, process.stderr);

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    process.removeAllListeners(signal);
    child.kill(signal);
  });
}

child.on("error", (error) => {
  throw error;
});

await waitForServer(url, child);
await writeFile(urlFile, `${url}\n`, "utf8");

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});

function reserveEphemeralPort() {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, LOOPBACK_ADDRESS, () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        server.close();
        reject(new Error("failed to reserve an IPv4 loopback port"));
        return;
      }
      server.close((error) => {
        if (error) {
          reject(error);
        } else {
          resolvePort(address.port);
        }
      });
    });
  });
}

async function waitForServer(url, childProcess) {
  const timeoutMs = Number.parseInt(
    process.env.SLIDEV_START_TIMEOUT_MS ?? "30000",
    10,
  );
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    if (childProcess.exitCode !== null || childProcess.signalCode !== null) {
      throw new Error("Slidev exited before its preview became ready");
    }

    if (await isServerReady(url)) {
      return;
    }

    await delay(100);
  }

  childProcess.kill("SIGTERM");
  throw new Error(`Slidev preview did not become ready within ${timeoutMs} ms`);
}

function isServerReady(url) {
  return new Promise((resolveReady) => {
    const request = httpGet(url, (response) => {
      response.resume();
      const status = response.statusCode ?? 0;
      resolveReady(status >= 200 && status < 400);
    });
    request.once("error", () => resolveReady(false));
    request.setTimeout(1000, () => {
      request.destroy();
      resolveReady(false);
    });
  });
}

function forwardOutput(stream, destination) {
  stream.on("data", (chunk) => {
    destination.write(chunk);
  });
}
