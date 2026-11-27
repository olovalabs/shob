import { chmod } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

export default defineConfig({
  build: {
    ssr: true,
    target: "node22",
    rolldownOptions: {
      input: {
        shob: fileURLToPath(new URL("./src/shob.ts", import.meta.url)),
        "shob-desktop": fileURLToPath(new URL("./src/shob-desktop.ts", import.meta.url)),
      },
      output: {
        entryFileNames: "[name].js",
        banner: (chunk) => (chunk.isEntry ? "#!/usr/bin/env node" : ""),
      },
      plugins: [
        {
          name: "cli-executable",
          async writeBundle({ dir }, bundle) {
            if (!dir) return;
            await Promise.all(
              Object.values(bundle)
                .filter((file) => file.type === "chunk" && file.isEntry)
                .map((file) => chmod(join(dir, file.fileName), 0o755)),
            );
          },
        },
      ],
    },
  },
});
