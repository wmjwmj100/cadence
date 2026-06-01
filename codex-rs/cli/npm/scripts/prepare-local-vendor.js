#!/usr/bin/env node

import { copyFileSync, chmodSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const packageRoot = path.resolve(path.dirname(__filename), "..");
const repoRoot = path.resolve(packageRoot, "..", "..");

function determineTargetTriple(platform, arch) {
  switch (platform) {
    case "linux":
    case "android":
      if (arch === "x64") {
        return "x86_64-unknown-linux-musl";
      }
      if (arch === "arm64") {
        return "aarch64-unknown-linux-musl";
      }
      break;
    case "darwin":
      if (arch === "x64") {
        return "x86_64-apple-darwin";
      }
      if (arch === "arm64") {
        return "aarch64-apple-darwin";
      }
      break;
    case "win32":
      if (arch === "x64") {
        return "x86_64-pc-windows-msvc";
      }
      if (arch === "arm64") {
        return "aarch64-pc-windows-msvc";
      }
      break;
    default:
      break;
  }
  return null;
}

const targetTriple = determineTargetTriple(process.platform, process.arch);
if (!targetTriple) {
  throw new Error(`Unsupported platform: ${process.platform} (${process.arch})`);
}

const binaryName = process.platform === "win32" ? "wecode.exe" : "wecode";
const candidates = [
  path.join(repoRoot, "target", targetTriple, "release", binaryName),
  path.join(repoRoot, "target", "release", binaryName),
  path.join(repoRoot, "target", "debug", binaryName),
];
const source = candidates.find((candidate) => existsSync(candidate));

if (!source) {
  console.error("Could not find a local wecode binary. Build one first, for example:");
  console.error("  cargo build --release -p codex-cli");
  console.error(`Looked in:\n${candidates.map((candidate) => `  ${candidate}`).join("\n")}`);
  process.exit(1);
}

const destinationDir = path.join(packageRoot, "vendor", targetTriple, "wecode");
const destination = path.join(destinationDir, binaryName);
mkdirSync(destinationDir, { recursive: true });
copyFileSync(source, destination);

if (process.platform !== "win32") {
  chmodSync(destination, 0o755);
}

console.log(`Copied ${source} -> ${destination}`);
