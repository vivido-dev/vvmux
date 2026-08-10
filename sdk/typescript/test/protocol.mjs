import assert from "node:assert/strict";
import test from "node:test";
import { PassThrough } from "node:stream";
import { readFrame, writeFrame } from "../dist/index.js";

test("big-endian frame round trip", async () => {
  const stream = new PassThrough();
  await writeFrame(stream, { type: "ready", request_id: 7 });
  assert.deepEqual(await readFrame(stream), { type: "ready", request_id: 7 });
});
