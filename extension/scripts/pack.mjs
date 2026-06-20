// Zip the built dist/ into snipdesk-extension.zip for Web Store upload
// or self-hosted distribution. Run after `npm run build`.
import { createWriteStream, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { resolve } from "node:path";

const dist = resolve(import.meta.dirname, "..", "dist");
const out = resolve(import.meta.dirname, "..", "snipdesk-extension.zip");

if (!existsSync(dist)) {
  console.error("dist/ not found - run `npm run build` first.");
  process.exit(1);
}

// Use the OS zip tools: PowerShell on Windows, `zip` elsewhere.
try {
  if (process.platform === "win32") {
    execFileSync(
      "powershell",
      [
        "-NoProfile",
        "-Command",
        `Compress-Archive -Path '${dist}\\*' -DestinationPath '${out}' -Force`,
      ],
      { stdio: "inherit" },
    );
  } else {
    execFileSync("zip", ["-r", out, "."], { cwd: dist, stdio: "inherit" });
  }
  console.info(`packed -> ${out}`);
} catch (err) {
  console.error("pack failed:", err.message);
  process.exit(1);
}
