import net from "node:net";
import fs from "node:fs";
import http from "node:http";
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const root = path.resolve(__dirname, "..");
const desktop = path.resolve(root, "apps", "desktop");

const host = "127.0.0.1";
const startPort = 5173;
const maxPort = 5300;
const electronMain = path.resolve(root, "electron-dist", "main.js");

function isPortFree(port) {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.once("error", () => resolve(false));
    server.once("listening", () => {
      server.close(() => resolve(true));
    });
    server.listen(port, host);
  });
}

async function findFreePort() {
  for (let port = startPort; port <= maxPort; port += 1) {
    if (await isPortFree(port)) return port;
  }
  throw new Error(`No free port found between ${startPort} and ${maxPort}`);
}

function waitForFile(filePath, timeoutMs = 60000) {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const tick = () => {
      if (fs.existsSync(filePath)) return resolve(true);
      if (Date.now() - started > timeoutMs) return reject(new Error(`Timeout waiting for ${filePath}`));
      setTimeout(tick, 200);
    };
    tick();
  });
}

function waitForHttp(url, timeoutMs = 60000) {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const tryRequest = () => {
      const req = http.get(url, (res) => {
        res.resume();
        if (res.statusCode && res.statusCode >= 200 && res.statusCode < 500) return resolve(true);
        retry();
      });
      req.on("error", retry);
    };
    const retry = () => {
      if (Date.now() - started > timeoutMs) return reject(new Error(`Timeout waiting for ${url}`));
      setTimeout(tryRequest, 300);
    };
    tryRequest();
  });
}

function spawnBun(args, cwd = root) {
  const bun = process.platform === "win32" ? "bun.exe" : "bun";
  const child = spawn(bun, args, {
    stdio: "inherit",
    shell: process.platform === "win32",
    env: { ...process.env },
    cwd,
  });
  return child;
}

async function ensureServerBundle() {
  const bundlePath = path.resolve(root, "packages", "server", "dist", "node", "node.js");
  if (fs.existsSync(bundlePath)) return;

  console.log("[dev] server bundle missing, building...");
  const { execFileSync } = await import("node:child_process");
  const scriptPath = path.resolve(__dirname, "build-server.mjs");
  try {
    execFileSync(process.execPath, [scriptPath], { stdio: "inherit" });
  } catch {
    console.error("[dev] failed to build server bundle");
    process.exit(1);
  }
}

async function main() {
  await ensureServerBundle();

  const port = await findFreePort();
  const devUrl = `http://${host}:${port}`;
  console.log(`[dev] using port ${port}`);

  const children = [];
  let shuttingDown = false;

  const shutdown = (code = 0) => {
    if (shuttingDown) return;
    shuttingDown = true;
    for (const child of children) {
      if (!child.killed) child.kill("SIGTERM");
    }
    setTimeout(() => process.exit(code), 200);
  };

  process.on("SIGINT", () => shutdown(0));
  process.on("SIGTERM", () => shutdown(0));

  // Run tsc for electron main process
  const tsc = spawnBun(["x", "tsc", "-p", path.join(root, "apps", "desktop", "tsconfig.electron.json"), "--watch", "--preserveWatchOutput"], root);
  children.push(tsc);
  tsc.on("exit", (code) => {
    if (!shuttingDown && code !== 0) shutdown(code || 1);
  });

  // Run vite dev server from desktop dir
  const vite = spawnBun(["x", "vite", "--host", host, "--port", String(port)], desktop);
  children.push(vite);
  vite.on("exit", (code) => {
    if (!shuttingDown && code !== 0) shutdown(code || 1);
  });

  await waitForFile(electronMain);
  await waitForHttp(devUrl);

  // Launch electron pointing to vite dev server
  const electron = spawnBun(["x", "cross-env", `VITE_DEV_SERVER_URL=${devUrl}`, "electron", "."], root);
  children.push(electron);

  electron.on("exit", (code) => {
    if (!shuttingDown) shutdown(code || 0);
  });
}

main().catch((error) => {
  console.error("[dev] failed:", error.message);
  process.exit(1);
});
