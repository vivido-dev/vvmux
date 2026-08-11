import { once } from "node:events";
import type { Readable, Writable } from "node:stream";

export const PROTOCOL_VERSION = 1;
export const MAX_FRAME_BYTES = 1024 * 1024;

export class ProtocolError extends Error {}

export class HostCallError extends Error {
  constructor(public readonly code: string, message: string) {
    super(message);
  }
}

async function readExact(stream: Readable, length: number): Promise<Buffer> {
  const chunks: Buffer[] = [];
  let total = 0;
  while (total < length) {
    const chunk = stream.read(length - total) as Buffer | null;
    if (chunk === null) {
      const ended = Promise.race([
        once(stream, "readable"),
        once(stream, "end").then(() => { throw new Error("unexpected EOF"); }),
      ]);
      await ended;
      continue;
    }
    chunks.push(chunk);
    total += chunk.length;
  }
  return Buffer.concat(chunks, total);
}

export async function readFrame(stream: Readable): Promise<Record<string, unknown>> {
  const prefix = await readExact(stream, 4);
  const length = prefix.readUInt32BE();
  if (length > MAX_FRAME_BYTES) throw new ProtocolError("native plugin frame exceeds 1 MiB");
  const value: unknown = JSON.parse((await readExact(stream, length)).toString("utf8"));
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new ProtocolError("native plugin frame must contain a JSON object");
  }
  return value as Record<string, unknown>;
}

export async function writeFrame(stream: Writable, value: unknown): Promise<void> {
  const body = Buffer.from(JSON.stringify(value));
  if (body.length > MAX_FRAME_BYTES) throw new ProtocolError("native plugin frame exceeds 1 MiB");
  const prefix = Buffer.allocUnsafe(4);
  prefix.writeUInt32BE(body.length);
  if (!stream.write(Buffer.concat([prefix, body]))) await once(stream, "drain");
}

export type Handler = (
  action: string,
  input: unknown,
  context: Record<string, unknown>,
) => unknown | Promise<unknown>;

export class NativeHost {
  private nextRequestId = 1;

  constructor(
    private readonly input: Readable,
    private readonly output: Writable,
  ) {}

  async call(method: string, params: unknown): Promise<unknown> {
    const requestId = this.nextRequestId++;
    await writeFrame(this.output, {
      type: "host_call",
      request_id: requestId,
      method,
      params,
    });
    const reply = await readFrame(this.input);
    if (reply.request_id !== requestId) throw new ProtocolError("host call reply ID does not match");
    if (reply.type === "host_call_result") return reply.result;
    if (reply.type === "host_call_error") {
      throw new HostCallError(String(reply.code), String(reply.message));
    }
    throw new ProtocolError(`unexpected host call reply: ${String(reply.type)}`);
  }
}

export type HostHandler = (
  action: string,
  input: unknown,
  context: Record<string, unknown>,
  host: NativeHost,
) => unknown | Promise<unknown>;

export async function serve(
  pluginId: string,
  instanceId: string,
  handler: Handler,
  input: Readable = process.stdin,
  output: Writable = process.stdout,
): Promise<void> {
  return serveWithHost(
    pluginId,
    instanceId,
    (action, value, context) => handler(action, value, context),
    input,
    output,
  );
}

export async function serveWithHost(
  pluginId: string,
  instanceId: string,
  handler: HostHandler,
  input: Readable = process.stdin,
  output: Writable = process.stdout,
): Promise<void> {
  await writeFrame(output, {
    type: "hello",
    protocol_version: PROTOCOL_VERSION,
    plugin_id: pluginId,
    instance_id: instanceId,
    features: [],
  });
  for (;;) {
    const message = await readFrame(input);
    const requestId = message.request_id;
    switch (message.type) {
      case "initialize":
        await writeFrame(output, { type: "ready", request_id: requestId });
        break;
      case "invoke":
        try {
          const result = await handler(
            message.action as string,
            message.input,
            message.context as Record<string, unknown>,
            new NativeHost(input, output),
          );
          await writeFrame(output, { type: "result", request_id: requestId, result });
        } catch (error) {
          await writeFrame(output, {
            type: "error",
            request_id: requestId,
            code: error instanceof HostCallError ? error.code : "runtime_crashed",
            message: error instanceof Error ? error.message : String(error),
          });
        }
        break;
      case "cancel":
        await writeFrame(output, { type: "cancelled", request_id: requestId });
        break;
      case "shutdown":
        await writeFrame(output, { type: "ready", request_id: requestId });
        return;
      default:
        throw new ProtocolError(`unexpected native plugin message: ${String(message.type)}`);
    }
  }
}
