// installed and managed by vvmux; reinstalling may overwrite this file.
// VVMUX_INTEGRATION_ID=opencode
// VVMUX_INTEGRATION_VERSION=2

import { execFile } from "node:child_process";

const SOURCE = "vvmux:opencode";
// vvmux bounds a block reason at 256 bytes and rejects the whole report if it is longer.
const MAX_MESSAGE_BYTES = 256;
let sequence = Date.now() * 1000;
let running = false;
let pending;
let lastQueued;
let agentSessionID;
let sessionSent = false;
const childSessions = new Set();

const STATUS_STATES = new Map([
  ["idle", "idle"],
  ["active", "working"],
  ["busy", "working"],
  ["pending", "working"],
  ["retry", "working"],
  ["running", "working"],
  ["streaming", "working"],
  ["working", "working"],
]);

function clampMessage(message) {
  if (typeof message !== "string") return undefined;
  const trimmed = message.replace(/\s+/g, " ").trim();
  if (!trimmed) return undefined;
  return Buffer.byteLength(trimmed) > MAX_MESSAGE_BYTES
    ? Buffer.from(trimmed).subarray(0, MAX_MESSAGE_BYTES).toString("utf8").replace(/�$/, "")
    : trimmed;
}

function queueState(state, message) {
  if (!state) return;
  const next = { state, message: clampMessage(message) };
  if (next.state === lastQueued?.state && next.message === lastQueued?.message) return;
  lastQueued = next;
  pending = next;
  pump();
}

function pump() {
  const binary = process.env.VVMUX_BIN;
  const session = process.env.VVMUX_SESSION;
  const pane = process.env.VVMUX_PANE_ID;
  if (running || !pending || !binary || !session || !pane) return;
  const { state, message } = pending;
  pending = undefined;
  running = true;
  sequence += 1;
  // Session identity is sent once; vvmux keeps it across later state-only reports.
  const identity =
    !sessionSent && agentSessionID ? ["--agent-session-id", agentSessionID] : [];
  const sending = identity.length > 0;
  execFile(
    binary,
    [
      "msg", "--target", session, "report-agent",
      "--agent", "opencode", "--state", state,
      "--source", SOURCE, "--sequence", String(sequence),
      ...(message ? ["--message", message] : []),
      ...identity,
      "--pane-id", pane,
    ],
    { timeout: 500, windowsHide: true },
    (error) => {
      if (sending && !error) sessionSent = true;
      running = false;
      pump();
    },
  );
}

function sessionID(properties) {
  return typeof properties?.sessionID === "string" ? properties.sessionID : undefined;
}

function blockReason(type, properties) {
  const detail =
    properties?.permission?.title ??
    properties?.question?.title ??
    properties?.title ??
    properties?.error?.message ??
    properties?.message;
  const label = type === "session.error" ? "error" : "waiting for approval";
  return typeof detail === "string" && detail ? `${label}: ${detail}` : label;
}

export const VvmuxAgentStatePlugin = async () => {
  if (!process.env.VVMUX_BIN || !process.env.VVMUX_SESSION || !process.env.VVMUX_PANE_ID) {
    return {};
  }
  return {
    "chat.message": async ({ sessionID }) => {
      if (!sessionID || !childSessions.has(sessionID)) queueState("working");
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const id = sessionID(properties);
      if (properties.info?.id && properties.info.parentID) childSessions.add(properties.info.id);
      if (id && childSessions.has(id)) {
        if (type === "permission.asked" || type === "question.asked") queueState("blocked", blockReason(type, properties));
        else if (["permission.replied", "question.replied", "question.rejected"].includes(type)) queueState("working");
        return;
      }
      // The first top-level session this pane sees owns the resumable conversation.
      if (id && !agentSessionID && !childSessions.has(id)) agentSessionID = id;
      switch (type) {
        case "session.status": {
          const kind = typeof properties.status === "string" ? properties.status : properties.status?.type;
          queueState(typeof kind === "string" ? STATUS_STATES.get(kind.toLowerCase()) : undefined);
          break;
        }
        case "permission.asked":
        case "question.asked":
        case "session.error":
          queueState("blocked", blockReason(type, properties));
          break;
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          queueState("working");
          break;
        case "session.idle":
          queueState("idle");
          break;
        case "session.updated":
          break;
        default:
          break;
      }
    },
  };
};
