import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readFile, rename, rm, unlink, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import { linuxDataDir } from "./release";

const exec = promisify(execFile);

async function readOptional(file: string) {
  return readFile(file, "utf8").catch((error) => {
    if (error.code !== "ENOENT") throw error;
  });
}

async function linuxDesktop(binary: string, install: boolean) {
  const file = join(linuxDataDir(), "applications", "shob.desktop");
  if (/[\r\n]/.test(binary)) throw new Error("unsupported newline in the Shob executable path");
  const quoted = binary
    .replaceAll("%", "%%")
    .replace(/["$`\\]/g, "\\$&")
    .replaceAll("\\", "\\\\");
  const command = `Exec=/usr/bin/env SHOB_SKIP_DESKTOP_ENTRY=1 "${quoted}" gui`;
  const entry = [
    "[Desktop Entry]",
    "Type=Application",
    "Name=Shob",
    "Comment=GPU-accelerated terminal",
    command,
    "Icon=utilities-terminal",
    "Terminal=false",
    "Categories=System;TerminalEmulator;",
    "StartupWMClass=shob",
    "",
  ].join("\n");
  const existing = await readOptional(file);
  if (existing !== undefined) {
    const commands = existing.split("\n").filter((line) => line.startsWith("Exec="));
    if (commands.length !== 1 || ![command, `Exec=${binary} gui`].includes(commands[0])) {
      throw new Error(`a different Shob desktop entry already exists at ${file}`);
    }
    if (!install) await unlink(file);
  } else if (install) {
    await mkdir(dirname(file), { recursive: true });
    await writeFile(file, entry, { flag: "wx" });
  }
  return file;
}

async function macDesktop(binary: string, install: boolean) {
  const folder = join(homedir(), "Applications");
  const app = join(folder, "Shob.app");
  const program = "Contents/MacOS/Shob";
  const wrapper = `#!/bin/sh\nexec '${binary.replaceAll("'", "'\\''")}' gui\n`;
  const existing = await readOptional(join(app, program));
  if (existing !== undefined) {
    if (existing !== wrapper) throw new Error(`a different Shob app already exists at ${app}`);
    if (!install) await rm(app, { recursive: true });
  } else if (install) {
    await mkdir(folder, { recursive: true });
    const staging = await mkdtemp(join(folder, ".shob-"));
    try {
      await mkdir(dirname(join(staging, program)), { recursive: true });
      await writeFile(join(staging, program), wrapper, { mode: 0o755 });
      await writeFile(join(staging, "Contents", "Info.plist"), `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>Shob</string>
  <key>CFBundleIdentifier</key><string>io.github.olovalabs.shob</string>
  <key>CFBundleName</key><string>Shob</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1</string>
</dict>
</plist>
`);
      await rename(staging, app);
    } finally {
      await rm(staging, { recursive: true, force: true });
    }
  }
  return app;
}

const windowsShortcut = String.raw`
$ErrorActionPreference = 'Stop'
$file = $env:SHOB_SHORTCUT
$binary = $env:SHOB_SHORTCUT_TARGET
$install = $env:SHOB_SHORTCUT_INSTALL -eq '1'
$shell = New-Object -ComObject WScript.Shell
if (Test-Path -LiteralPath $file) {
    $shortcut = $shell.CreateShortcut($file)
    if ($shortcut.TargetPath -ne $binary -or $shortcut.Arguments -ne 'gui') {
        throw "A different Shob shortcut already exists at $file"
    }
    if (-not $install) { Remove-Item -LiteralPath $file }
} elseif ($install) {
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($file)) | Out-Null
    $shortcut = $shell.CreateShortcut($file)
    $shortcut.TargetPath = $binary
    $shortcut.Arguments = 'gui'
    $shortcut.IconLocation = "$binary,0"
    $shortcut.WindowStyle = 7
    $shortcut.Save()
}
`;

async function windowsDesktop(binary: string, install: boolean) {
  const { APPDATA, SystemRoot } = process.env;
  if (!APPDATA || !SystemRoot) throw new Error("Windows APPDATA or SystemRoot is unavailable");
  const file = join(APPDATA, "Microsoft", "Windows", "Start Menu", "Programs", "Shob.lnk");
  const powershell = join(SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe");
  await exec(powershell, ["-NoProfile", "-NonInteractive", "-Command", windowsShortcut], {
    windowsHide: true,
    env: {
      ...process.env,
      SHOB_SHORTCUT: file,
      SHOB_SHORTCUT_TARGET: binary,
      SHOB_SHORTCUT_INSTALL: install ? "1" : "0",
    },
  });
  return file;
}

export async function registerDesktop(binary: string, install: boolean) {
  const register = {
    linux: linuxDesktop,
    darwin: macDesktop,
    win32: windowsDesktop,
  };
  const platform = process.platform;
  if (platform !== "linux" && platform !== "darwin" && platform !== "win32") {
    throw new Error(`desktop registration is unavailable on ${platform}`);
  }
  const file = await register[platform](binary, install);
  console.log(install ? `Desktop entry installed at ${file}` : `Desktop entry removed from ${file}`);
}
