import { spawnSync } from "node:child_process";

export interface TestRequest {
  command: [string, ...string[]];
  cwd?: string;
}

export function runTests(request: TestRequest) {
  return spawnSync(request.command[0], request.command.slice(1), {
    cwd: request.cwd,
    shell: false,
    encoding: "utf8",
    timeout: 30_000,
    maxBuffer: 256 * 1024,
  });
}

// agent-utility.mjs is the dependency-free checked-in build used by the manifest.
