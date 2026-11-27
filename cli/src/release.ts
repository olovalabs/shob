import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { createWriteStream } from "node:fs";
import { chmod, link, lstat, mkdir, mkdtemp, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";
import { Readable, Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { promisify } from "node:util";

const exec = promisify(execFile);
const repository = "olovalabs/shob";
const maxPayload = 256 * 1024 * 1024;
const maxMetadata = 2 * 1024 * 1024;

interface ReleaseTarget {
  asset: string;
  archiveMember: string | null;
  binary: string;
}

interface ReleaseAsset {
  tag: string;
  size: number;
  digest: string;
  url: string;
}

interface GitHubRelease {
  tag_name?: string;
  assets?: { name: string; size?: number; digest?: string }[];
}

export function linuxDataDir() {
  const xdg = process.env.XDG_DATA_HOME;
  return xdg && isAbsolute(xdg) ? xdg : join(homedir(), ".local", "share");
}

export function releaseTarget(): ReleaseTarget {
  let triple;
  let dataDir;
  switch (`${process.platform}-${process.arch}`) {
    case "linux-x64":
      triple = "x86_64-unknown-linux-gnu";
      dataDir = join(linuxDataDir(), "shob");
      break;
    case "darwin-arm64":
      triple = "aarch64-apple-darwin";
      dataDir = join(homedir(), "Library", "Application Support", "Shob");
      break;
    case "win32-x64":
      triple = "x86_64-pc-windows-msvc";
      dataDir = join(process.env.LOCALAPPDATA || join(homedir(), "AppData", "Local"), "Shob");
      break;
    default:
      throw new Error(
        `no release for ${process.platform}/${process.arch}. See https://github.com/${repository} for source builds.`,
      );
  }
  const windows = process.platform === "win32";
  return {
    asset: `shob-${triple}.${windows ? "exe" : "tar.gz"}`,
    archiveMember: windows ? null : `shob-${triple}/shob`,
    binary: join(dataDir, "launcher", triple, windows ? "shob.exe" : "shob"),
  };
}

async function get(url: string, timeout: number) {
  const response = await fetch(url, {
    headers: { "User-Agent": "shob-npm-launcher" },
    signal: AbortSignal.timeout(timeout),
  });
  if (!response.ok || !response.url.startsWith("https://") || !response.body) {
    await response.body?.cancel();
    throw new Error(`request failed (HTTP ${response.status}) for ${url}`);
  }
  return Readable.fromWeb(response.body);
}

async function latestAsset(name: string): Promise<ReleaseAsset> {
  const source = await get(`https://api.github.com/repos/${repository}/releases/latest`, 15_000);
  let size = 0;
  const chunks: Buffer[] = [];
  for await (const chunk of source) {
    size += chunk.length;
    if (size > maxMetadata) throw new Error("GitHub release metadata is too large");
    chunks.push(chunk);
  }
  const release: GitHubRelease = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  const tag = release.tag_name;
  const asset = release.assets?.find((item) => item.name === name);
  if (typeof tag !== "string" || !/^v\d+\.\d+\.\d+$/.test(tag) || !asset ||
      typeof asset.size !== "number" || !Number.isSafeInteger(asset.size) ||
      asset.size <= 0 || asset.size > maxPayload || typeof asset.digest !== "string" ||
      !/^sha256:[a-f0-9]{64}$/i.test(asset.digest)) {
    throw new Error(`no verified ${name} asset in the latest Shob release`);
  }
  return {
    tag,
    size: asset.size,
    digest: asset.digest.slice("sha256:".length).toLowerCase(),
    url: `https://github.com/${repository}/releases/download/${tag}/${name}`,
  };
}

async function download(asset: ReleaseAsset, dest: string) {
  const hash = createHash("sha256");
  let size = 0;
  const verify = new Transform({
    transform(chunk, _encoding, done) {
      size += chunk.length;
      if (size > asset.size) {
        done(new Error("release download exceeded the expected size"));
        return;
      }
      hash.update(chunk);
      done(null, chunk);
    },
  });
  await pipeline(await get(asset.url, 300_000), verify, createWriteStream(dest, { flags: "wx" }));
  if (size !== asset.size || hash.digest("hex") !== asset.digest) {
    throw new Error("release download failed SHA-256 verification");
  }
}

export async function ensureBinary({ asset, archiveMember, binary }: ReleaseTarget) {
  const existing = await lstat(binary).catch((error) => {
    if (error.code !== "ENOENT") throw error;
  });
  if (existing) {
    if (!existing.isFile()) throw new Error(`${binary} is not a regular file`);
    return;
  }

  const directory = dirname(binary);
  await mkdir(directory, { recursive: true });
  const staging = await mkdtemp(join(directory, ".download-"));
  try {
    const selected = await latestAsset(asset);
    console.error(`Downloading Shob ${selected.tag}…`);
    const payload = join(staging, asset);
    await download(selected, payload);
    let executable = payload;
    if (archiveMember) {
      await exec("tar", ["-xzf", payload, "-C", staging, "--", archiveMember]);
      executable = join(staging, archiveMember);
      if (!(await lstat(executable)).isFile()) throw new Error("invalid Shob release executable");
      await chmod(executable, 0o755);
    }
    // Publish a complete binary without overwriting another launcher's install or an update.
    await link(executable, binary).catch(async (error) => {
      if (error.code !== "EEXIST" || !(await lstat(binary)).isFile()) throw error;
    });
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}
