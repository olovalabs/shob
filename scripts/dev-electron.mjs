import net from "node:net";
import fs from "node:fs";
import http from "node:http";
import { spawn } from "node:child_process";
import path from "node:path";

const host = "127.0.0.1";
const startPort = 5173;
const maxPort = 5300;
const electronMain = path.resolve("electron-dist/main.js");

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

function spawnCmd(command, args, extraEnv = {}) {
  const child = spawn(command, args, {
    stdio: "inherit",
    shell: process.platform === "win32",
    env: { ...process.env, ...extraEnv },
  });
  return child;
}

function spawnPnpm(args, extraEnv = {}) {
  const npmExecPath = process.env.npm_execpath;
  if (npmExecPath && (npmExecPath.endsWith(".js") || npmExecPath.endsWith(".cjs"))) {
    return spawnCmd(process.execPath, [npmExecPath, ...args], extraEnv);
  }
  return spawnCmd(process.platform === "win32" ? "pnpm.cmd" : "pnpm", args, extraEnv);
}

async function main() {
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

  const tsc = spawnPnpm(["run", "dev:electron:tsc"]);
  children.push(tsc);
  tsc.on("exit", (code) => {
    if (!shuttingDown && code !== 0) shutdown(code || 1);
  });

  const vite = spawnPnpm(["exec", "vite", "--host", host, "--port", String(port)]);
  children.push(vite);
  vite.on("exit", (code) => {
    if (!shuttingDown && code !== 0) shutdown(code || 1);
  });

  await waitForFile(electronMain);
  await waitForHttp(devUrl);

  const electron = spawnPnpm(["exec", "cross-env", `VITE_DEV_SERVER_URL=${devUrl}`, "electron", "."], {
    VITE_DEV_SERVER_URL: devUrl,
  });
  children.push(electron);

  electron.on("exit", (code) => {
    if (!shuttingDown) shutdown(code || 0);
  });
}

main().catch((error) => {
  console.error("[dev] failed:", error.message);
  process.exit(1);
});
