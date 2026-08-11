import assert from "node:assert/strict";
import test from "node:test";
import { PassThrough } from "node:stream";
import { NativeHost, readFrame, writeFrame } from "../dist/index.js";

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
