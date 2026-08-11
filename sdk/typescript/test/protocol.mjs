import assert from "node:assert/strict";
import test from "node:test";
import { PassThrough } from "node:stream";
import { NativeHost, readFrame, serveWithEvents, writeFrame } from "../dist/index.js";

test("big-endian frame round trip", async () => {
  const stream = new PassThrough();
  await writeFrame(stream, { type: "ready", request_id: 7 });
  assert.deepEqual(await readFrame(stream), { type: "ready", request_id: 7 });
});

test("native host emits and correlates broker calls", async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  await writeFrame(input, { type: "host_call_result", request_id: 1, result: { ok: true } });
  const request = readFrame(output);
  const result = await new NativeHost(input, output).call("session.inspect", {});
  assert.deepEqual(result, { ok: true });
  assert.deepEqual(await request, {
    type: "host_call",
    request_id: 1,
    method: "session.inspect",
    params: {},
  });
});

test("service dispatches event frames and acknowledges completion", async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  const seen = [];
  const serving = serveWithEvents(
    "dev.events",
    "instance-a",
    async () => ({}),
    async (name, payload, context) => seen.push({ name, payload, context }),
    input,
    output,
  );
  assert.equal((await readFrame(output)).type, "hello");
  await writeFrame(input, { type: "initialize", request_id: 1 });
  assert.deepEqual(await readFrame(output), { type: "ready", request_id: 1 });
  await writeFrame(input, {
    type: "event",
    request_id: 2,
    sequence: 7,
    name: "pane-exited",
    payload: { pane_id: 3 },
    context: { causation_depth: 1 },
  });
  assert.deepEqual(await readFrame(output), { type: "ready", request_id: 2 });
  assert.deepEqual(seen, [
    { name: "pane-exited", payload: { pane_id: 3 }, context: { causation_depth: 1 } },
  ]);
  await writeFrame(input, { type: "shutdown", request_id: 3 });
  assert.deepEqual(await readFrame(output), { type: "ready", request_id: 3 });
  await serving;
});
