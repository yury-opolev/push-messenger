#!/usr/bin/env node
// Serves the calls example and proxies the API to a push-messenger deployment, so the
// page, GET /v1/calls/ice-servers and the WebSocket share one origin (the API sends no
// CORS headers). No dependencies.
//
//   node serve.mjs --api <http(s)://deployment> [--port 8080] [--host 127.0.0.1]
//
// Browsers allow microphone access (getUserMedia) only in a secure context: open the page
// on http://127.0.0.1 or http://localhost, or put HTTPS in front for other hosts.
// It prints "listening on <url>" once it accepts connections.

import { createReadStream } from "node:fs";
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const files = {
  "/": ["index.html", "text/html; charset=utf-8"],
  "/index.html": ["index.html", "text/html; charset=utf-8"],
  "/calls.js": ["calls.js", "text/javascript; charset=utf-8"],
  "/frame.proto": ["../../../crates/push-proto/proto/push/v1/frame.proto", "text/plain; charset=utf-8"],
};

function option(name, fallback) {
  const at = process.argv.indexOf(`--${name}`);
  return at > 0 && process.argv[at + 1] ? process.argv[at + 1] : fallback;
}

const api = new URL(option("api", process.env.PUSH_API_URL ?? ""));
if (!["http:", "https:"].includes(api.protocol)) {
  console.error("usage: node serve.mjs --api <http(s)://deployment> [--port 8080] [--host 127.0.0.1]");
  process.exit(2);
}
const secure = api.protocol === "https:";
const apiPort = Number(api.port) || (secure ? 443 : 80);

const server = http.createServer((request, response) => {
  const path = new URL(request.url, "http://local").pathname;
  const file = files[path];
  if (file && request.method === "GET") {
    response.writeHead(200, { "content-type": file[1], "cache-control": "no-store" });
    createReadStream(join(here, file[0])).pipe(response);
    return;
  }
  if (!path.startsWith("/v1/")) {
    response.writeHead(404).end();
    return;
  }
  const upstream = (secure ? https : http).request(
    {
      host: api.hostname,
      port: apiPort,
      method: request.method,
      path: request.url,
      headers: { ...request.headers, host: api.host },
    },
    (answer) => {
      response.writeHead(answer.statusCode, answer.headers);
      answer.pipe(response);
    },
  );
  upstream.on("error", () => response.headersSent ? response.destroy() : response.writeHead(502).end());
  request.pipe(upstream);
});

// The WebSocket: the upgrade request is replayed to the API, then bytes flow both ways.
server.on("upgrade", (request, socket, head) => {
  const upstream = secure
    ? tls.connect({ host: api.hostname, port: apiPort, servername: api.hostname })
    : net.connect({ host: api.hostname, port: apiPort });
  upstream.once(secure ? "secureConnect" : "connect", () => {
    const headers = { ...request.headers, host: api.host };
    delete headers.origin;
    let text = `${request.method} ${request.url} HTTP/1.1\r\n`;
    for (const [name, value] of Object.entries(headers)) text += `${name}: ${value}\r\n`;
    upstream.write(`${text}\r\n`);
    if (head.length) upstream.write(head);
    upstream.pipe(socket);
    socket.pipe(upstream);
  });
  const close = () => {
    socket.destroy();
    upstream.destroy();
  };
  upstream.on("error", close);
  socket.on("error", close);
  upstream.on("close", close);
  socket.on("close", close);
});

server.listen(Number(option("port", "8080")), option("host", "127.0.0.1"), () => {
  const { address, port } = server.address();
  console.log(`listening on http://${address}:${port}/ (API ${api.origin})`);
});
