// installed and managed by vvmux; reinstalling may overwrite this file.
// VVMUX_INTEGRATION_ID=opencode
// VVMUX_INTEGRATION_VERSION=1

import { execFile } from "node:child_process";

const SOURCE = "vvmux:opencode";
let sequence = Date.now() * 1000;
let running = false;
let pending;
let lastQueued;
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

function queueState(state) {
  if (!state || state === lastQueued) return;
  lastQueued = state;
  pending = state;
  pump();
}

function pump() {
  const binary = process.env.VVMUX_BIN;
  const session = process.env.VVMUX_SESSION;
  const pane = process.env.VVMUX_PANE_ID;
  if (running || !pending || !binary || !session || !pane) return;
  const state = pending;
  pending = undefined;
  running = true;
  sequence += 1;
  execFile(
    binary,
    [
      "msg", "--target", session, "report-agent",
      "--agent", "opencode", "--state", state,
      "--source", SOURCE, "--sequence", String(sequence),
      "--pane-id", pane,
    ],
    { timeout: 500, windowsHide: true },
    () => {
      running = false;
      pump();
    },
  );
}

function sessionID(properties) {
  return typeof properties?.sessionID === "string" ? properties.sessionID : undefined;
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
        if (type === "permission.asked" || type === "question.asked") queueState("blocked");
        else if (["permission.replied", "question.replied", "question.rejected"].includes(type)) queueState("working");
        return;
      }
      switch (type) {
        case "session.status": {
          const kind = typeof properties.status === "string" ? properties.status : properties.status?.type;
          queueState(typeof kind === "string" ? STATUS_STATES.get(kind.toLowerCase()) : undefined);
          break;
        }
        case "permission.asked":
        case "question.asked":
        case "session.error":
          queueState("blocked");
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
