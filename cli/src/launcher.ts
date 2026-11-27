import { spawn } from "node:child_process";
import { once } from "node:events";
import { registerDesktop } from "./desktop";
import { ensureBinary, releaseTarget } from "./release";

export async function launch(args: string[]) {
  try {
    const desktop = args[0] === "gui";
    const action = desktop ? args[1] : undefined;
    if (desktop && (action === "--help" || action === "-h") && args.length === 2) {
      console.log("Usage: shob gui [install|remove]");
      return;
    }
    if (desktop && (args.length > 2 || (action && action !== "install" && action !== "remove"))) {
      throw new Error("usage: shob gui [install|remove]");
    }

    const target = releaseTarget();
    if (action === "remove") {
      await registerDesktop(target.binary, false);
      return;
    }
    await ensureBinary(target);
    if (action === "install") {
      await registerDesktop(target.binary, true);
      return;
    }

    const child = spawn(target.binary, args.length ? args : ["tui"], {
      detached: desktop,
      stdio: desktop ? "ignore" : "inherit",
      windowsHide: desktop,
      env: desktop ? { ...process.env, SHOB_SKIP_DESKTOP_ENTRY: "1" } : process.env,
    });
    if (desktop) {
      await once(child, "spawn");
      child.unref();
    } else {
      const [code, signal] = await once(child, "exit");
      if (signal && process.platform !== "win32") {
        process.kill(process.pid, signal);
      }
      process.exitCode = code ?? 1;
    }
  } catch (error) {
    console.error(`shob: ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  }
}
