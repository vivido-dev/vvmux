#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { performance } from "node:perf_hooks";

let body = "";
for await (const chunk of process.stdin) body += chunk;
const input = JSON.parse(body);
const operation = process.argv[2];

if (operation === "run-tests") {
  if (!Array.isArray(input.command) || input.command.length === 0) {
    throw new Error("command must be a non-empty argv array");
  }
  const started = performance.now();
  const result = spawnSync(input.command[0], input.command.slice(1), {
    cwd: input.cwd,
    shell: false,
    encoding: "utf8",
    timeout: 30_000,
    maxBuffer: 256 * 1024,
  });
  const durationMs = Math.ceil(performance.now() - started);
  process.stdout.write(
    JSON.stringify({
      success: result.status === 0,
      status: result.status,
      stdout: (result.stdout ?? "").slice(0, 131_072),
      stderr: (result.stderr ?? result.error?.message ?? "").slice(0, 131_072),
      duration_ms: durationMs,
    }),
  );
} else if (operation === "summarize") {
  const result = input.result;
  const detail = result.success
    ? "Checks passed"
    : `Checks failed${result.status === null ? "" : ` with status ${result.status}`}: ${result.stderr || result.stdout}`;
  process.stdout.write(
    JSON.stringify({
      summary: detail.slice(0, 512),
      durations: [result.duration_ms],
    }),
  );
} else {
  throw new Error(`unknown action ${operation}`);
}
