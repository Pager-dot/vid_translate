// Wrapper around the Tauri CLI: `npm run tauri <args>` lands here, not on `tauri` directly.
//
// DO NOT REMOVE, and do not point the "tauri" npm script back at the bare CLI. On Windows
// the first build would then fail deep inside whisper-rs-sys with the misleading
//
//   CMake Error: No CMAKE_C_COMPILER could be found.   (→ MSB8066, then a panic)
//
// The real cause is path length. whisper.cpp's Vulkan backend compiles its shader generator
// as a nested CMake project, and MSBuild's file tracker writes logs about 200 characters
// below Cargo's build directory. Under `src-tauri\target` that comes to over 260 characters
// (266 on a C:\Users\<name>\Desktop checkout), and MSBuild has no long-path support, so
// enabling LongPathsEnabled in Windows does not help. The fix is to move Cargo's
// intermediate files to a short root via CARGO_BUILD_BUILD_DIR. The app, installers and
// bundles still go to src-tauri\target\release, so nothing downstream moves.
//
// It sits here rather than in src-tauri/.cargo/config.toml because that file is shared by
// Linux and macOS and cannot hold a Windows-only path. CI sets the same variable in its
// workflows; a value that is already set (CI, `setx`, a shell export) is always respected.
//
// Linux and macOS are passed straight through unchanged.

import { spawn } from "node:child_process";
import { createRequire } from "node:module";

const env = { ...process.env };

if (process.platform === "win32" && !env.CARGO_BUILD_BUILD_DIR) {
  env.CARGO_BUILD_BUILD_DIR = `${env.SystemDrive || "C:"}\\vtb`;
  console.log(
    `[tauri wrapper] CARGO_BUILD_BUILD_DIR=${env.CARGO_BUILD_BUILD_DIR} ` +
      "(Windows path-length workaround, see scripts/tauri.mjs)",
  );
}

// The CLI's own entry script, run with this same Node: no shell, so arguments reach it
// verbatim on every OS (a shell would re-split anything containing spaces).
const cli = createRequire(import.meta.url).resolve("@tauri-apps/cli/tauri.js");
const child = spawn(process.execPath, [cli, ...process.argv.slice(2)], { stdio: "inherit", env });
child.on("exit", (code, signal) => process.exit(signal ? 1 : (code ?? 1)));
