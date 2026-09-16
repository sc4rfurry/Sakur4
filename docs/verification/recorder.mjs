#!/usr/bin/env node
/**
 * A recording reverse proxy: forwards to llama.cpp and dumps what it received.
 *
 * Used to see the exact body the Sakur4 proxy forwards, which is the artifact every
 * assertion about rewriting is really about. It writes each request as pretty JSON to a
 * directory so a failing run can be inspected after the fact rather than only live.
 *
 * Usage:
 *   node docs/verification/recorder.mjs --listen 8775 --upstream http://host:8080 --dump /tmp/dump
 */

import { createServer } from "node:http";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const LISTEN = Number(arg("listen", "8775"));
const UPSTREAM = arg("upstream", "http://100.98.158.87:8080");
const DUMP = arg("dump", null);

if (DUMP) mkdirSync(DUMP, { recursive: true });

let count = 0;
const server = createServer((request, response) => {
  let body = "";
  request.on("data", (chunk) => (body += chunk));
  request.on("end", async () => {
    if (request.url?.includes("chat/completions")) {
      count += 1;
      let summary = `${request.url}  ${body.length} bytes`;
      try {
        const parsed = JSON.parse(body);
        const messages = parsed.messages ?? [];
        const markers = messages.filter(
          (m) => typeof m.content === "string" && m.content.includes("[Sakur4 removed"),
        ).length;
        summary = `${messages.length} messages, ${markers} marker(s), ${body.length} bytes`;
        if (DUMP) {
          writeFileSync(join(DUMP, `request-${String(count).padStart(3, "0")}.json`), JSON.stringify(messages, null, 2));
        }
      } catch {
        /* a body that is not JSON is still worth counting */
      }
      console.log(`  [recorder] ${summary}`);
    }

    try {
      const forwarded = await fetch(`${UPSTREAM}${request.url}`, {
        method: request.method,
        headers: { "content-type": "application/json" },
        body: request.method === "POST" ? body : undefined,
      });
      const text = await forwarded.text();
      response.writeHead(forwarded.status, { "content-type": "application/json" });
      response.end(text);
    } catch (error) {
      response.writeHead(502);
      response.end(JSON.stringify({ error: String(error) }));
    }
  });
});

server.listen(LISTEN, "127.0.0.1", () => {
  console.log(`recorder listening on http://127.0.0.1:${LISTEN} -> ${UPSTREAM}`);
});
