#!/usr/bin/env node

const childProcess = require("child_process");
const fs = require("fs");
const os = require("os");
const path = require("path");

const packageJson = require("../package.json");

function target() {
  if (process.platform === "linux" && process.arch === "x64") {
    return { archive: "taskr-linux-x86_64.tar.gz" };
  }

  if (process.platform === "darwin" && process.arch === "arm64") {
    return { archive: "taskr-darwin-arm64.tar.gz" };
  }

  if (process.platform === "darwin" && process.arch === "x64") {
    return { archive: "taskr-darwin-x86_64.tar.gz" };
  }

  console.error(`Unsupported platform for @mmux/taskr: ${process.platform}/${process.arch}`);
  process.exit(1);
}

const selected = target();
const packageRoot = path.resolve(__dirname, "..");
const archivePath = path.join(packageRoot, "artifacts", selected.archive);
const cacheDir = path.join(
  os.tmpdir(),
  "taskr-npm",
  `${packageJson.version}-${process.platform}-${process.arch}`
);
const binaryPath = path.join(cacheDir, "taskr");

if (!fs.existsSync(archivePath)) {
  console.error(`Missing taskr archive: ${archivePath}`);
  process.exit(1);
}

if (!fs.existsSync(binaryPath)) {
  fs.rmSync(cacheDir, { recursive: true, force: true });
  fs.mkdirSync(cacheDir, { recursive: true });

  const extract = childProcess.spawnSync("tar", ["-xzf", archivePath, "-C", cacheDir], {
    stdio: "inherit"
  });

  if (extract.status !== 0) {
    process.exit(extract.status || 1);
  }

  if (!fs.existsSync(binaryPath)) {
    console.error(`Archive ${selected.archive} did not contain a taskr binary`);
    process.exit(1);
  }
  fs.chmodSync(binaryPath, 0o755);
}

const result = childProcess.spawnSync(binaryPath, process.argv.slice(2), {
  stdio: "inherit"
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

if (result.signal) {
  process.kill(process.pid, result.signal);
}

process.exit(result.status === null ? 1 : result.status);
