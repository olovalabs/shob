import { launch } from "./launcher";

await launch(["gui", ...process.argv.slice(2)]);
