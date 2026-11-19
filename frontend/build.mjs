import { cp, mkdir } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const output = resolve(root, "../cloud-platform/src/main/resources/static/vendor");
const edgeOutput = resolve(root, "../edge-node/static/vendor");
await mkdir(resolve(output, "fontawesome/css"), { recursive: true });
await mkdir(resolve(output, "fontawesome/webfonts"), { recursive: true });
await mkdir(edgeOutput, { recursive: true });

const tailwind = resolve(root, "node_modules/tailwindcss/lib/cli.js");
const cssResult = spawnSync(process.execPath, [
  tailwind,
  "-c", resolve(root, "tailwind.config.js"),
  "-i", resolve(root, "tailwind.css"),
  "-o", resolve(output, "app.css"),
  "--minify"
], { stdio: "inherit" });
if (cssResult.status !== 0) process.exit(cssResult.status ?? 1);

await cp(resolve(root, "node_modules/vue/dist/vue.global.prod.js"), resolve(output, "vue.global.prod.js"));
await cp(resolve(root, "node_modules/echarts/dist/echarts.min.js"), resolve(output, "echarts.min.js"));
await cp(resolve(root, "node_modules/@fortawesome/fontawesome-free/css/all.min.css"), resolve(output, "fontawesome/css/all.min.css"));
await cp(resolve(root, "node_modules/@fortawesome/fontawesome-free/webfonts"), resolve(output, "fontawesome/webfonts"), { recursive: true });
await cp(resolve(output, "app.css"), resolve(edgeOutput, "app.css"));
await cp(resolve(output, "vue.global.prod.js"), resolve(edgeOutput, "vue.global.prod.js"));
