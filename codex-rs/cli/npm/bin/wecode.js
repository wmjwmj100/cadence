#!/usr/bin/env node

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

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

function candidateBinaryPaths(targetTriple, platform) {
  const binaryName = platform === "win32" ? "wecode.exe" : "wecode";
  const packageRoot = path.join(__dirname, "..");
  const vendorRoot = path.join(packageRoot, "vendor");
  const archRoot = path.join(vendorRoot, targetTriple);

  return [
    process.env.WECODE_BINARY_PATH,
    path.join(archRoot, "wecode", binaryName),
    path.join(archRoot, binaryName),
  ].filter(Boolean);
}

const runtimePlatform = process.env.WECODE_NPM_TEST_PLATFORM ?? process.platform;
const runtimeArch = process.env.WECODE_NPM_TEST_ARCH ?? process.arch;
const targetTriple = determineTargetTriple(runtimePlatform, runtimeArch);
if (!targetTriple) {
  console.error(`Unsupported platform: ${runtimePlatform} (${runtimeArch})`);
  process.exit(1);
}

const binaryPath = candidateBinaryPaths(targetTriple, runtimePlatform).find(
  (candidate) => existsSync(candidate),
);

if (!binaryPath) {
  console.error(
    `Wecode binary for ${targetTriple} was not found in this npm package.`,
  );
  console.error(
    "Please reinstall @gradence/wecode or install a release that supports your platform.",
  );
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: false,
});

child.on("error", (err) => {
  console.error(err);
  process.exit(1);
});

const forwardSignal = (signal) => {
  if (!child.killed) {
    try {
      child.kill(signal);
    } catch {
      // The child may have already exited.
    }
  }
};

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => forwardSignal(signal));
}

const childResult = await new Promise((resolve) => {
  child.on("exit", (code, signal) => {
    if (signal) {
      resolve({ type: "signal", signal });
    } else {
      resolve({ type: "code", exitCode: code ?? 1 });
    }
  });
});

if (childResult.type === "signal") {
  process.kill(process.pid, childResult.signal);
} else {
  process.exit(childResult.exitCode);
}
