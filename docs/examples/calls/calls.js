// A minimal browser client for push-messenger voice calls (docs/calls.md, "Browser
// recipe"). It speaks push.v1 over the WebSocket with protobuf.js and the repository's
// frame.proto, gets its ICE servers from GET /v1/calls/ice-servers, trickles ICE, and
// follows the client rules: only the caller makes the initial offer, renegotiation uses
// perfect negotiation with the callee polite, peer candidates are queued until the remote
// description is set, and after every reconnect CallSync decides what survives (an
// active call restarts ICE). Sign-in, token refresh and contacts are the app's job: this
// client takes an access token.
//
// Usage: const client = new CallClient({ apiBase, token, protoUrl }); await client.connect();
// client.call(peerUserId) / client.answer(id) / client.decline(id) / client.hangup(id).
// It emits "status", "call" (a call changed) and "log" events through `onEvent`.

/* global protobuf */

const CAPABILITY_CALLS = "CAPABILITY_CALLS";
// Close codes after which reconnecting cannot help: 4001-4003 (the token is bad or
// expired) and 4007 (this device connected elsewhere).
const FINAL_CLOSE = (code) => (code >= 4001 && code <= 4003) || code === 4007;

class CallGone extends Error {}
const RECONNECT_FIRST_MS = 500;
const RECONNECT_MAX_MS = 8000;

// ---- identifiers -------------------------------------------------------------------

export function uuidToBytes(text) {
  const hex = text.replace(/-/g, "");
  if (!/^[0-9a-fA-F]{32}$/.test(hex)) throw new Error(`not a UUID: ${text}`);
  const bytes = new Uint8Array(16);
  for (let i = 0; i < 16; i += 1) bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return bytes;
}

export function bytesToUuid(bytes) {
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

// A UUIDv7 (RFC 9562): 48 bits of Unix milliseconds, then random bits. CallInvite
// requires version 7.
export function uuidv7() {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  let ms = Date.now();
  for (let i = 5; i >= 0; i -= 1) {
    bytes[i] = ms % 256;
    ms = Math.floor(ms / 256);
  }
  bytes[6] = 0x70 | (bytes[6] & 0x0f);
  bytes[8] = 0x80 | (bytes[8] & 0x3f);
  return bytesToUuid(bytes);
}

// ---- the client --------------------------------------------------------------------

export class CallClient {
  constructor({ apiBase, token, protoUrl = "frame.proto", relayOnly = false, onEvent = () => {} }) {
    this.apiBase = apiBase.replace(/\/$/, "");
    this.token = token;
    this.protoUrl = protoUrl;
    this.relayOnly = relayOnly;
    this.onEvent = onEvent;
    this.calls = new Map(); // call id (text) -> call
    this.userId = null;
    this.deviceId = null;
    this.callsAvailable = false;
    this.connected = false;
    this.closedByUser = false;
    this.reconnects = 0;
    this.ws = null;
    this.Frame = null;
    this.pingTimer = null;
  }

  log(text) {
    this.onEvent("log", `${new Date().toISOString().slice(11, 23)} ${text}`);
  }

  async connect() {
    if (!this.Frame) {
      const source = await (await fetch(this.protoUrl)).text();
      this.Frame = protobuf.parse(source, { keepCase: true }).root.lookupType("push.v1.Frame");
    }
    this.closedByUser = false;
    await this.open();
  }

  close() {
    this.closedByUser = true;
    for (const call of this.calls.values()) this.release(call);
    this.calls.clear();
    if (this.ws) this.ws.close(1000);
  }

  // One WebSocket; resolves once Ready arrived. Reconnects with backoff when it drops.
  open() {
    return new Promise((resolve, reject) => {
      const url = `${this.apiBase.replace(/^http/, "ws")}/v1/socket`;
      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";
      this.ws = ws;
      let ready = false;
      ws.onopen = () => {
        // Identify first: CAPABILITY_CALLS says this device can take calls.
        this.send({ identify: { token: this.token, capabilities: [CAPABILITY_CALLS] } });
      };
      ws.onmessage = (event) => {
        const frame = this.decode(event.data);
        if (frame.body === "ready") {
          ready = true;
          this.onReady(frame.ready);
          resolve();
          return;
        }
        this.onFrame(frame);
      };
      ws.onclose = (event) => {
        clearInterval(this.pingTimer);
        this.connected = false;
        this.status(`disconnected (${event.code})`);
        if (!ready) {
          const error = new Error(`the socket closed before Ready (${event.code})`);
          error.closeCode = event.code;
          reject(error);
        }
        // A connection that never got Ready is retried by reconnect() itself.
        if (ready && !this.closedByUser && !FINAL_CLOSE(event.code)) this.reconnect();
      };
    });
  }

  async reconnect(delay = RECONNECT_FIRST_MS) {
    await new Promise((r) => setTimeout(r, delay * (0.5 + Math.random())));
    if (this.closedByUser) return;
    try {
      await this.open();
      this.reconnects += 1;
    } catch (error) {
      this.log(`reconnect failed: ${error.message}`);
      if (!this.closedByUser && !FINAL_CLOSE(error.closeCode)) this.reconnect(Math.min(delay * 2, RECONNECT_MAX_MS));
    }
  }

  onReady(ready) {
    this.connected = true;
    this.userId = bytesToUuid(ready.user_id);
    this.deviceId = bytesToUuid(ready.device_id);
    this.callsAvailable = (ready.capabilities || []).includes(CAPABILITY_CALLS);
    // Keep the connection alive: a Ping every heartbeat.
    this.pingTimer = setInterval(() => this.send({ ping: {} }), ready.heartbeat_ms || 30000);
    this.status(this.callsAvailable ? "ready" : "ready (calls not offered by this server)");
  }

  status(text) {
    this.log(text);
    this.onEvent("status", text);
  }

  send(body) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return false;
    const message = this.Frame.fromObject(body);
    this.ws.send(this.Frame.encode(message).finish());
    return true;
  }

  decode(data) {
    const message = this.Frame.decode(new Uint8Array(data));
    return this.Frame.toObject(message, { enums: String, longs: Number, oneofs: true });
  }

  changed(call) {
    this.onEvent("call", call);
  }

  // ---- ICE servers -----------------------------------------------------------------

  // GET /v1/calls/ice-servers: the browser-ready iceServers list. Fetched for every call
  // and before an ICE restart (the TURN credentials are short-lived).
  async iceServers() {
    const response = await fetch(`${this.apiBase}/v1/calls/ice-servers`, {
      headers: { Authorization: `Bearer ${this.token}` },
    });
    if (!response.ok) {
      const body = await response.json().catch(() => ({}));
      throw new Error(`ice-servers: ${response.status} ${body.error?.code ?? ""}`);
    }
    return response.json(); // { ice_servers, expires_at, relay }
  }

  // Throws CallGone when the call ended while this client awaited something (a CallEnded,
  // a hang-up click), so no media or frame outlives it.
  alive(call) {
    if (call.status === "ended") {
      this.release(call);
      throw new CallGone();
    }
  }

  async peerConnection(call) {
    const servers = await this.iceServers();
    this.alive(call);
    if (this.relayOnly && !servers.relay) throw new Error("relay-only asked for, but the server offers no TURN relay");
    const pc = new RTCPeerConnection({
      iceServers: servers.ice_servers,
      // "relay" hides both sides' addresses from each other (docs/calls.md, Privacy).
      iceTransportPolicy: this.relayOnly ? "relay" : "all",
    });
    call.pc = pc;
    // The permission prompt can take a while: the call may end meanwhile.
    call.stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });
    this.alive(call);
    for (const track of call.stream.getTracks()) pc.addTrack(track, call.stream);
    pc.ontrack = (event) => {
      call.remoteStream = event.streams[0] ?? new MediaStream([event.track]);
      this.changed(call);
    };
    pc.onicecandidate = ({ candidate }) => this.localCandidate(call, candidate);
    pc.onconnectionstatechange = () => {
      this.log(`call ${call.id}: ${pc.connectionState}`);
      this.changed(call);
      // A broken path: restart ICE (the polite and the impolite side may both try;
      // perfect negotiation resolves the glare).
      if (pc.connectionState === "failed" && call.status === "active") this.restartIce(call);
    };
    // Renegotiation (ICE restart, later media changes) only once the call is active: the
    // initial offer travels in CallInvite and the answer in CallAccept.
    pc.onnegotiationneeded = async () => {
      if (call.status !== "active") return;
      try {
        call.makingOffer = true;
        await pc.setLocalDescription();
        this.sendDescription(call);
      } catch (error) {
        this.log(`renegotiation failed: ${error.message}`);
      } finally {
        call.makingOffer = false;
      }
    };
    return pc;
  }

  // CallRenegotiate with the local description, typed by what it is: an implicit
  // setLocalDescription() can make an answer when the peer's offer arrived first.
  sendDescription(call) {
    const { type, sdp } = call.pc.localDescription;
    this.send({
      call_renegotiate: { call_id: call.idBytes, sdp, type: type === "offer" ? "CALL_SDP_TYPE_OFFER" : "CALL_SDP_TYPE_ANSWER" },
    });
  }

  // ---- trickle ICE -----------------------------------------------------------------

  localCandidate(call, candidate) {
    const frame = candidate && candidate.candidate
      ? {
          call_id: call.idBytes,
          candidate: candidate.candidate,
          sdp_mid: candidate.sdpMid ?? undefined,
          sdp_mline_index: candidate.sdpMLineIndex ?? undefined,
          username_fragment: candidate.usernameFragment ?? undefined,
        }
      : { call_id: call.idBytes, candidate: "", end_of_candidates: true };
    // The caller may trickle from the invite on; the callee only after its CallAccept.
    if (call.canSendIce) this.send({ call_ice: frame });
    else call.outgoingIce.push(frame);
  }

  flushLocalCandidates(call) {
    call.canSendIce = true;
    for (const frame of call.outgoingIce.splice(0)) this.send({ call_ice: frame });
  }

  async peerCandidate(call, ice) {
    // Candidates may arrive before CallAccepted (caller) or the offer (callee) is applied:
    // queue them until the remote description is set.
    if (!call.pc || !call.pc.remoteDescription) {
      call.incomingIce.push(ice);
      return;
    }
    try {
      if (ice.end_of_candidates) await call.pc.addIceCandidate();
      else {
        await call.pc.addIceCandidate({
          candidate: ice.candidate,
          sdpMid: ice.sdp_mid,
          sdpMLineIndex: ice.sdp_mline_index,
          usernameFragment: ice.username_fragment,
        });
      }
    } catch (error) {
      if (!call.ignoreOffer) this.log(`addIceCandidate: ${error.message}`);
    }
  }

  async flushPeerCandidates(call) {
    for (const ice of call.incomingIce.splice(0)) await this.peerCandidate(call, ice);
  }

  // ---- placing and answering -------------------------------------------------------

  newCall(id, fields) {
    const call = {
      id,
      idBytes: uuidToBytes(id),
      pc: null,
      stream: null,
      remoteStream: null,
      outgoingIce: [],
      incomingIce: [],
      canSendIce: false,
      makingOffer: false,
      ignoreOffer: false,
      ringTimer: null,
      ...fields,
    };
    this.calls.set(id, call);
    this.changed(call);
    return call;
  }

  async call(peerUserId) {
    const id = uuidv7();
    // The caller is impolite; it alone makes the initial offer.
    const call = this.newCall(id, { role: "caller", polite: false, peerUserId, status: "calling" });
    try {
      const pc = await this.peerConnection(call);
      await pc.setLocalDescription(await pc.createOffer());
      this.alive(call);
      this.send({
        call_invite: {
          call_id: call.idBytes,
          callee_user_id: uuidToBytes(peerUserId),
          offer_sdp: pc.localDescription.sdp,
          media: ["CALL_MEDIA_AUDIO"],
        },
      });
      this.flushLocalCandidates(call);
      call.status = "ringing";
      this.changed(call);
    } catch (error) {
      if (error instanceof CallGone) return id;
      this.log(`call failed: ${error.message}`);
      this.end(call, "FAILED (local)");
    }
    return id;
  }

  async answer(id) {
    const call = this.calls.get(id);
    if (!call || call.status !== "incoming") return;
    clearTimeout(call.ringTimer);
    call.status = "answering";
    this.changed(call);
    try {
      const pc = await this.peerConnection(call);
      await pc.setRemoteDescription({ type: "offer", sdp: call.offerSdp });
      await pc.setLocalDescription(await pc.createAnswer());
      this.alive(call);
      this.send({ call_accept: { call_id: call.idBytes, answer_sdp: pc.localDescription.sdp } });
      // Only now may the callee send its candidates.
      this.flushLocalCandidates(call);
      call.status = "active";
      this.changed(call);
      await this.flushPeerCandidates(call);
    } catch (error) {
      if (error instanceof CallGone) return;
      this.log(`answer failed: ${error.message}`);
      this.send({ call_hangup: { call_id: call.idBytes, reason: "CALL_END_REASON_FAILED" } });
    }
  }

  decline(id, busy = false) {
    const call = this.calls.get(id);
    if (!call) return;
    this.send({ call_decline: { call_id: call.idBytes, reason: busy ? "CALL_END_REASON_BUSY" : "CALL_END_REASON_DECLINED" } });
  }

  // Cancel while ringing (caller), hang up once active (either side).
  hangup(id) {
    const call = this.calls.get(id);
    if (!call) return;
    // Not invited yet (still preparing media): the server knows nothing, end it here.
    if (call.status === "calling") this.end(call, "CANCELLED (local)");
    else if (call.role === "caller" && call.status !== "active") this.send({ call_cancel: { call_id: call.idBytes } });
    else this.send({ call_hangup: { call_id: call.idBytes } });
  }

  // An offer with new ICE credentials, sent as CallRenegotiate. Made explicitly rather
  // than through restartIce() and negotiationneeded, which does not fire outside the
  // stable state: a caller that missed CallAccepted while disconnected still has its
  // original offer pending, and this new offer replaces it.
  async restartIce(call) {
    if (!call.pc) return;
    try {
      // Fresh TURN credentials first: the old ones may have expired.
      const servers = await this.iceServers();
      if (!call.pc) return;
      call.pc.setConfiguration({ ...call.pc.getConfiguration(), iceServers: servers.ice_servers });
    } catch (error) {
      this.log(`ice-servers before the restart: ${error.message}`);
    }
    const pc = call.pc;
    // An offer of the peer is being answered: that answer goes out first.
    if (!pc || pc.signalingState === "have-remote-offer") return;
    this.log(`call ${call.id}: ICE restart`);
    try {
      call.makingOffer = true;
      await pc.setLocalDescription(await pc.createOffer({ iceRestart: true }));
      this.sendDescription(call);
    } catch (error) {
      this.log(`ICE restart failed: ${error.message}`);
    } finally {
      call.makingOffer = false;
    }
  }

  release(call) {
    clearTimeout(call.ringTimer);
    if (call.stream) for (const track of call.stream.getTracks()) track.stop();
    if (call.pc) call.pc.close();
    call.pc = null;
  }

  end(call, reason, durationS) {
    this.release(call);
    call.status = "ended";
    call.endReason = reason;
    call.durationS = durationS;
    this.calls.delete(call.id);
    this.log(`call ${call.id} ended: ${reason}${durationS !== undefined ? ` after ${durationS} s` : ""}`);
    this.changed(call);
  }

  // ---- server frames ---------------------------------------------------------------

  onFrame(frame) {
    const body = frame[frame.body];
    const call = body && body.call_id ? this.calls.get(bytesToUuid(body.call_id)) : undefined;
    switch (frame.body) {
      case "call_incoming":
        this.incoming(body);
        break;
      case "call_accepted":
        if (call) this.accepted(call, body);
        break;
      case "call_taken":
        // Another device of ours answered: stop ringing here.
        if (call) this.end(call, "TAKEN (answered on another device)");
        break;
      case "call_ended":
        if (call) this.end(call, String(body.reason).replace("CALL_END_REASON_", ""), body.duration_s);
        break;
      case "call_error":
        this.callError(call, body);
        break;
      case "call_sync":
        this.sync(body.calls || []);
        break;
      case "peer_call_ice":
        if (call) this.peerCandidate(call, body);
        break;
      case "peer_call_renegotiate":
        if (call) this.renegotiate(call, body);
        break;
      case "call_history_changed":
        this.onEvent("history", body);
        break;
      case "auth_expiring":
        // A real app refreshes the access token over HTTPS and sends ReAuth here.
        this.log(`the access token expires in ${body.expires_in_s} s (ReAuth is the app's job)`);
        break;
      case "error":
        this.log(`error ${body.code}: ${body.message}`);
        break;
      default:
        break;
    }
  }

  incoming(body, fromSync = false) {
    const id = bytesToUuid(body.call_id);
    if (this.calls.has(id)) return;
    const call = this.newCall(id, {
      role: "callee",
      polite: true,
      peerUserId: bytesToUuid(fromSync ? body.peer_user_id : body.caller_user_id),
      offerSdp: body.offer_sdp,
      status: "incoming",
    });
    // Ring for expires_in_ms from now, never by the device clock against expires_at.
    call.ringTimer = setTimeout(() => {
      if (call.status === "incoming") this.end(call, "MISSED (local timeout)");
    }, body.expires_in_ms);
    this.log(`incoming call ${id} from ${call.peerUserId}`);
  }

  async accepted(call, body) {
    if (call.status === "active") return; // a resent CallAccepted
    call.status = "active";
    this.changed(call);
    await call.pc.setRemoteDescription({ type: "answer", sdp: body.answer_sdp });
    await this.flushPeerCandidates(call);
  }

  callError(call, body) {
    this.log(`call error ${body.code}: ${body.message}`);
    if (!call) return;
    switch (body.code) {
      case "ERROR_CODE_ALREADY_ANSWERED":
        // A cancel lost the race: the call is connected, hang it up.
        if (call.role === "caller") this.send({ call_hangup: { call_id: call.idBytes } });
        else this.end(call, "TAKEN (answered on another device)");
        break;
      case "ERROR_CODE_UNAVAILABLE":
      case "ERROR_CODE_CALL_LIMIT":
        // Retryable or a capped frame (ICE, renegotiation): the call goes on.
        break;
      case "ERROR_CODE_INVALID":
        // One refused CallIce or CallRenegotiate does not end an answered call.
        if (call.status === "active") break;
        this.end(call, "ERROR INVALID");
        break;
      default:
        // CALL_NOT_FOUND, NOT_CONTACT, INVALID, IN_CALL, RATE_LIMITED, NOT_AVAILABLE:
        // the call does not exist (any more) for this device.
        this.end(call, `ERROR ${String(body.code).replace("ERROR_CODE_", "")}`);
    }
  }

  // Perfect negotiation (https://developer.mozilla.org/docs/Web/API/WebRTC_API/Perfect_negotiation).
  async renegotiate(call, body) {
    const pc = call.pc;
    if (!pc) return;
    const description = { type: body.type === "CALL_SDP_TYPE_OFFER" ? "offer" : "answer", sdp: body.sdp };
    const collision = description.type === "offer" && (call.makingOffer || pc.signalingState !== "stable");
    call.ignoreOffer = !call.polite && collision;
    if (call.ignoreOffer) return; // the impolite caller keeps its own offer
    try {
      await pc.setRemoteDescription(description); // a polite callee rolls back implicitly
      if (description.type === "offer") {
        await pc.setLocalDescription();
        this.sendDescription(call);
      }
      await this.flushPeerCandidates(call);
    } catch (error) {
      this.log(`renegotiate: ${error.message}`);
    }
  }

  // After every Ready: what the server still knows decides what survives.
  sync(states) {
    const listed = new Set(states.map((state) => bytesToUuid(state.call_id)));
    for (const call of [...this.calls.values()]) {
      if (!listed.has(call.id)) this.end(call, "GONE (not in CallSync)");
    }
    for (const state of states) {
      const id = bytesToUuid(state.call_id);
      const call = this.calls.get(id);
      const ringingIn = state.direction === "CALL_DIRECTION_IN" && state.status === "CALL_STATUS_RINGING";
      if (ringingIn && !call) {
        this.incoming(state, true);
      } else if (state.status === "CALL_STATUS_ACTIVE" && !state.this_device) {
        // Answered on another device while this one was away.
        if (call) this.end(call, "TAKEN (answered on another device)");
      } else if (state.status === "CALL_STATUS_ACTIVE" && state.this_device) {
        if (call && call.pc) {
          // A caller that missed CallAccepted while away: the call is active now, and
          // the restart's offer replaces the unanswered one.
          call.status = "active";
          this.changed(call);
          this.restartIce(call);
        } else {
          // Active here but without media (the page was reloaded): end it.
          this.send({ call_hangup: { call_id: state.call_id, reason: "CALL_END_REASON_FAILED" } });
        }
      }
    }
    this.log(`CallSync: ${states.length} call(s)`);
  }
}
