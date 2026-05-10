import fs from "node:fs";
import path from "node:path";
import { execSync } from "node:child_process";
import os from "node:os";

const __dirname = path.dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Z]:)/, "$1"));
const rootDir = path.resolve(__dirname, "..");
const serverDir = path.resolve(rootDir, "packages", "server");
const serverDistDir = path.resolve(serverDir, "dist", "node");
const serverBundle = path.resolve(serverDistDir, "node.js");

if (fs.existsSync(serverBundle)) {
  console.log("[build-server] server bundle already exists, skipping build");
  process.exit(0);
}

console.log("[build-server] server bundle not found, building...");

function findBun() {
  const bunName = process.platform === "win32" ? "bun.exe" : "bun";
  if (process.env.BUN_INSTALL) {
    const candidate = path.join(process.env.BUN_INSTALL, bunName);
    if (fs.existsSync(candidate)) return candidate;
  }
  const home = process.env.HOME || process.env.USERPROFILE;
  if (home) {
    const candidate = path.join(home, ".bun", "bin", bunName);
    if (fs.existsSync(candidate)) return candidate;
  }
  return bunName;
}

function findMonorepoServerDir() {
  const candidates = [
    path.resolve(rootDir, "..", "opencode", "opencode", "packages", "opencode"),
    path.resolve(rootDir, "..", "opencode", "packages", "opencode"),
    path.resolve(os.homedir(), "Desktop", "opencode", "opencode", "packages", "opencode"),
  ];
  if (process.env.OPENCODE_MONOREPO_PATH) {
    candidates.unshift(path.resolve(process.env.OPENCODE_MONOREPO_PATH, "packages", "opencode"));
  }
  for (const candidate of candidates) {
    const buildScript = path.resolve(candidate, "script", "build-node.ts");
    if (fs.existsSync(buildScript)) return candidate;
  }
  return null;
}

function tryBuildFromMonorepo(bun) {
  const monorepoDir = findMonorepoServerDir();
  if (!monorepoDir) {
    console.log("[build-server] OpenCode monorepo not found, cannot build from monorepo");
    return false;
  }

  console.log(`[build-server] found monorepo at: ${monorepoDir}`);
  console.log("[build-server] building server from monorepo...");

  try {
    execSync([bun, "run", "script/build-node.ts"].join(" "), {
      cwd: monorepoDir,
      stdio: "inherit",
      env: { ...process.env },
    });
  } catch {
    console.error("[build-server] monorepo build failed");
    return false;
  }

  const monorepoDist = path.resolve(monorepoDir, "dist", "node");
  if (!fs.existsSync(path.resolve(monorepoDist, "node.js"))) {
    console.error("[build-server] monorepo build succeeded but output not found");
    return false;
  }

  fs.mkdirSync(serverDistDir, { recursive: true });
  for (const file of fs.readdirSync(monorepoDist)) {
    fs.copyFileSync(
      path.resolve(monorepoDist, file),
      path.resolve(serverDistDir, file),
    );
  }

  console.log("[build-server] copied build artifacts from monorepo to vendor directory");
  return true;
}

const bun = findBun();
console.log(`[build-server] using bun: ${bun}`);

if (tryBuildFromMonorepo(bun)) {
  if (fs.existsSync(serverBundle)) {
    console.log("[build-server] server bundle built successfully (from monorepo)");
    process.exit(0);
  }
}

console.error("[build-server] failed to build server bundle");
console.error("[build-server] either:");
console.error("[build-server]   1. Run 'bun run script/build-node.ts' from the OpenCode monorepo to generate the bundle, then copy dist/node/ to packages/server/dist/node/");
console.error("[build-server]   2. Set OPENCODE_MONOREPO_PATH env var to point to the OpenCode monorepo root");
console.error("[build-server]   3. Ensure the pre-built bundle exists in packages/server/dist/node/");
process.exit(1);