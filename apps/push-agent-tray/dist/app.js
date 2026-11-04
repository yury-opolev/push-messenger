// The tray window: the agent list, the selected agent's Overview, Chat, Logs and Settings
// tabs, and the tray's own settings. It talks to the tray only through its commands.
// Text only: the DOM is built with createElement and textContent, never HTML.
"use strict";

const REFRESH_MS = 2000;
const LOG_POLL_MS = 1000;
const RETRY_MS = 2000;
const LOG_KEEP = 500;
const TABS = ["overview", "chat", "logs", "settings"];
const LEVEL_RANK = { info: 0, warn: 1, error: 2 };

const state = {
  agents: [],
  selected: null,
  tab: "overview",
  agent: null,
  actionErrors: {},
  logLines: [],
  logDirty: true,
};

let refreshTimer = null;
let refreshing = false;
let refreshAgain = false;
let chatRun = 0;
let logRun = 0;

function $(id) {
  return document.getElementById(id);
}

function invoke(command, args) {
  return window.__TAURI_INTERNALS__.invoke(command, args || {});
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function visible() {
  return document.visibilityState === "visible";
}

function text(value) {
  return value === null || value === undefined || value === "" ? "—" : String(value);
}

function errorText(error) {
  if (typeof error === "string") return error;
  if (error && typeof error.message === "string") return error.message;
  return String(error);
}

function el(tag, className, content) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (content !== undefined) node.textContent = content;
  return node;
}

function badgeClass(key) {
  return "badge badge-" + String(key || "grey");
}

function dateTime(ms) {
  const date = new Date(ms);
  return Number.isNaN(date.getTime()) ? "" : date.toLocaleString();
}

function clockTime(ms) {
  const date = new Date(ms);
  return Number.isNaN(date.getTime()) ? "" : date.toLocaleTimeString();
}

function setProblem(id, error) {
  $(id).textContent = error ? errorText(error) : "";
}

// ---- Window view: agent list and footer -------------------------------------------------

async function refresh() {
  clearTimeout(refreshTimer);
  refreshTimer = null;
  try {
    const reply = await invoke("window_view");
    renderWindow(reply.view);
    if (reply.select && state.agents.some((agent) => agent.id === reply.select)) {
      select(reply.select, false);
    }
    if (state.selected) {
      await refreshAgent();
    }
    setProblem("globalProblem", null);
  } catch (error) {
    setProblem("globalProblem", error);
  }
  if (visible()) {
    refreshTimer = setTimeout(refresh, REFRESH_MS);
  }
}

function renderWindow(view) {
  state.agents = view.agents || [];
  renderAgentList();
  const global = view.global || {};
  $("startAtLogon").checked = Boolean(global.startAtLogon);
  $("reduceMotion").checked = Boolean(global.reduceMotion);
  document.body.classList.toggle("reduce-motion", Boolean(global.reduceMotion));
  $("menuPeek").checked = Boolean(global.menuPeek);
  $("switchOver").hidden = !global.switchOffered;
  const notice = $("configError");
  notice.hidden = !global.configError;
  notice.textContent = global.configError ? "tray.json: " + global.configError : "";
  if (state.selected && !state.agents.some((agent) => agent.id === state.selected)) {
    select(state.agents.length ? state.agents[0].id : null, false);
  } else if (!state.selected && state.agents.length) {
    select(state.agents[0].id, false);
  }
}

const rows = new Map();

function agentRow(agent) {
  let row = rows.get(agent.id);
  if (!row) {
    const button = el("button", "agent-row");
    button.type = "button";
    button.setAttribute("role", "option");
    button.dataset.id = agent.id;
    const badge = el("span");
    const name = el("span", "name");
    const stateLabel = el("span", "state");
    const folder = el("span", "folder");
    button.append(badge, name, stateLabel, folder);
    button.addEventListener("click", () => select(agent.id, false));
    button.addEventListener("keydown", onAgentKey);
    const item = el("li");
    item.setAttribute("role", "none");
    item.append(button);
    row = { item, button, badge, name, stateLabel, folder };
    rows.set(agent.id, row);
  }
  row.badge.className = badgeClass(agent.badge);
  row.name.textContent = text(agent.name);
  row.stateLabel.textContent = agent.call ? text(agent.state) + " · " + agent.call : text(agent.state);
  row.folder.textContent = text(agent.folder);
  const selected = agent.id === state.selected;
  row.button.setAttribute("aria-selected", selected ? "true" : "false");
  row.button.tabIndex = selected || (!state.selected && agent === state.agents[0]) ? 0 : -1;
  return row;
}

function renderAgentList() {
  const list = $("agents");
  const ids = new Set(state.agents.map((agent) => agent.id));
  for (const [id, row] of rows) {
    if (!ids.has(id)) {
      row.item.remove();
      rows.delete(id);
    }
  }
  state.agents.forEach((agent, index) => {
    const row = agentRow(agent);
    if (list.children[index] !== row.item) {
      list.insertBefore(row.item, list.children[index] || null);
    }
  });
  $("noAgents").hidden = state.agents.length > 0;
}

function onAgentKey(event) {
  const index = state.agents.findIndex((agent) => agent.id === event.currentTarget.dataset.id);
  let next = -1;
  if (event.key === "ArrowDown") next = Math.min(index + 1, state.agents.length - 1);
  else if (event.key === "ArrowUp") next = Math.max(index - 1, 0);
  else if (event.key === "Home") next = 0;
  else if (event.key === "End") next = state.agents.length - 1;
  else return;
  event.preventDefault();
  if (next >= 0 && next !== index) {
    select(state.agents[next].id, true);
  }
}

// ---- Selection and tabs -----------------------------------------------------------------

function select(id, focus) {
  const changed = id !== state.selected;
  state.selected = id;
  for (const agent of state.agents) agentRow(agent);
  if (focus && id && rows.has(id)) rows.get(id).button.focus();
  $("agent").hidden = !id;
  $("empty").hidden = Boolean(id);
  if (!changed) return;
  state.agent = null;
  state.actionErrors = {};
  state.logLines = [];
  state.logDirty = true;
  $("chat").replaceChildren();
  $("chatState").textContent = "";
  $("log").replaceChildren();
  hideRemoveConfirm();
  for (const problem of document.querySelectorAll("main .refused")) problem.textContent = "";
  if (id) {
    refreshAgent();
  }
  startTabLoops();
}

function showTab(tab, focus) {
  state.tab = tab;
  for (const name of TABS) {
    const button = $("tab-" + name);
    const selected = name === tab;
    button.setAttribute("aria-selected", selected ? "true" : "false");
    button.tabIndex = selected ? 0 : -1;
    $("panel-" + name).hidden = !selected;
    if (selected && focus) button.focus();
  }
  if (tab === "logs") state.logDirty = true;
  startTabLoops();
}

function onTabKey(event) {
  const index = TABS.indexOf(state.tab);
  let next = -1;
  if (event.key === "ArrowRight") next = (index + 1) % TABS.length;
  else if (event.key === "ArrowLeft") next = (index - 1 + TABS.length) % TABS.length;
  else if (event.key === "Home") next = 0;
  else if (event.key === "End") next = TABS.length - 1;
  else return;
  event.preventDefault();
  showTab(TABS[next], true);
}

function startTabLoops() {
  chatRun += 1;
  logRun += 1;
  if (!state.selected || !visible()) return;
  if (state.tab === "chat") chatLoop(chatRun, state.selected);
  if (state.tab === "logs") logLoop(logRun, state.selected);
}

// ---- Overview and Settings ----------------------------------------------------------------

async function refreshAgent() {
  const id = state.selected;
  if (!id) return;
  try {
    const agent = await invoke("agent_view", { id });
    if (id !== state.selected) return;
    state.agent = agent;
    renderAgent(agent);
  } catch (error) {
    if (id === state.selected) setProblem("openFolderProblem", error);
  }
}

const actionButtons = new Map();
const callButtons = [];

function actionWrapper(onClick) {
  const wrapper = el("span", "action");
  const button = el("button");
  button.type = "button";
  button.addEventListener("click", onClick);
  const refused = el("span", "refused");
  refused.setAttribute("role", "status");
  wrapper.append(button, refused);
  return { wrapper, button, refused };
}

function actionControl(action) {
  let control = actionButtons.get(action.action);
  if (!control) {
    control = actionWrapper(() => act(action.action));
    $("actions").append(control.wrapper);
    actionButtons.set(action.action, control);
  }
  return control;
}

// The call's buttons by place: the first toggles between "mute" and "unmute".
function callControl(index) {
  let control = callButtons[index];
  if (!control) {
    control = actionWrapper(() => {
      if (control.action) act(control.action);
    });
    control.action = null;
    $("callActions").append(control.wrapper);
    callButtons[index] = control;
  }
  return control;
}

function renderAction(control, action) {
  control.button.textContent = action.label;
  control.button.disabled = !action.enabled;
  const local = state.actionErrors[action.action];
  control.refused.textContent = local || action.refused || "";
}

function renderAgent(agent) {
  $("agentBadge").className = badgeClass(agent.badge);
  $("agentName").textContent = text(agent.name);
  $("ovBadge").className = badgeClass(agent.badge);
  $("ovState").textContent = text(agent.state);
  for (const action of agent.actions || []) {
    renderAction(actionControl(action), action);
  }
  const callActions = agent.callActions || [];
  callActions.forEach((action, index) => {
    const control = callControl(index);
    control.action = action.action;
    renderAction(control, action);
  });
  callButtons.forEach((control, index) => {
    control.wrapper.hidden = index >= callActions.length;
  });
  $("ovCall").textContent = agent.call ? agent.call : "no call";
  $("ovVoiceAdapterLabel").hidden = !agent.voiceAdapter;
  $("ovVoiceAdapter").hidden = !agent.voiceAdapter;
  $("ovVoiceAdapter").textContent = text(agent.voiceAdapter);
  $("ovFolder").textContent = text(agent.folder);
  $("ovSession").textContent = text(agent.session);
  $("ovCodaVersion").textContent = text(agent.codaVersion);
  $("ovUptime").textContent = text(agent.uptime);
  $("ovRunningTasks").textContent = text(agent.runningTasks);
  $("ovLastError").textContent = text(agent.lastError);
  $("ovLastExit").textContent = text(agent.lastExit);
  const history = (agent.history || []).map((entry) => {
    const item = el("li");
    const at = el("time", null, dateTime(entry.at));
    item.append(at, el("span", null, text(entry.state)));
    return item;
  });
  $("history").replaceChildren(...history);

  const settings = agent.settings || {};
  $("setAutostart").checked = Boolean(settings.autostart);
  $("setShowPeek").checked = Boolean(settings.showPeek);
  const messages = $("setPeekMessages");
  if (document.activeElement !== messages) messages.value = String(settings.peekMessages || 20);
  $("configPath").textContent = text(settings.configPath);
}

async function act(action) {
  const id = state.selected;
  if (!id) return;
  state.actionErrors = {};
  try {
    await invoke("act", { id, action });
  } catch (error) {
    state.actionErrors[action] = errorText(error);
  }
  await refreshAgent();
}

async function call(problemId, command, args) {
  setProblem(problemId, null);
  try {
    return await invoke(command, args);
  } catch (error) {
    setProblem(problemId, error);
    return undefined;
  }
}

async function setAgentSetting(key, value) {
  const id = state.selected;
  if (!id) return;
  await call("settingsProblem", "set_agent_setting", { id, key, value });
  await refreshAgent();
}

function hideRemoveConfirm() {
  $("removeConfirm").hidden = true;
  $("remove").hidden = false;
}

// ---- Chat -------------------------------------------------------------------------------

function renderChat(reply) {
  $("openMessenger").hidden = !reply.messengerUrl;
  let status = "";
  if (reply.off) status = "Chat peek is off for this agent in Settings.";
  else if (reply.disabled)
    status = 'The bridge has the chat peek turned off ("peek": false in config.json).';
  else if (reply.error) status = reply.error;
  else if (!(reply.entries || []).length) status = "No messages yet.";
  $("chatState").textContent = status;
  const list = $("chat");
  const scroller = $("panel-chat").closest("main");
  const atBottom = scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 4;
  const entries = reply.off || reply.disabled ? [] : reply.entries || [];
  list.replaceChildren(...window.PushTrayChat.chatItems(document, entries));
  if (atBottom) scroller.scrollTop = scroller.scrollHeight;
}

async function chatLoop(run, id) {
  let seen = null;
  while (run === chatRun && state.selected === id && state.tab === "chat" && visible()) {
    try {
      const reply = await invoke("peek", { id, seen });
      if (run !== chatRun) return;
      setProblem("chatProblem", null);
      renderChat(reply);
      seen = reply.seen;
      if (reply.off || reply.disabled || reply.error) await sleep(RETRY_MS);
    } catch (error) {
      if (run !== chatRun) return;
      if (errorText(error) !== "hidden") setProblem("chatProblem", error);
      await sleep(RETRY_MS);
    }
  }
}

// ---- Logs -------------------------------------------------------------------------------

function logMatches(line, level, search) {
  const wanted = LEVEL_RANK[level] || 0;
  const rank = line.level ? LEVEL_RANK[String(line.level).toLowerCase()] || 0 : -1;
  const shown = wanted === 0 || rank >= wanted;
  return shown && (search === "" || String(line.raw).toLowerCase().includes(search.toLowerCase()));
}

function visibleLogLines() {
  const level = $("logLevel").value;
  const search = $("logSearch").value;
  return state.logLines.filter((line) => logMatches(line, level, search));
}

function renderLog() {
  const box = $("log");
  const atBottom = box.scrollTop + box.clientHeight >= box.scrollHeight - 4;
  const raw = $("logRaw").checked;
  const rowsOut = visibleLogLines().map((line) => {
    const row = el("div", "log-row");
    if (raw || (!line.level && !line.event && !line.time)) {
      row.textContent = line.raw;
      return row;
    }
    const level = String(line.level || "").toLowerCase();
    row.append(
      el("span", "time", line.time || ""),
      el("span", "level level-" + (LEVEL_RANK[level] !== undefined ? level : "other"), line.level || ""),
      el("span", "event", line.event || ""),
      el("span", "fields", line.fields || ""),
    );
    return row;
  });
  box.replaceChildren(...rowsOut);
  if (atBottom || state.logDirty) box.scrollTop = box.scrollHeight;
  state.logDirty = false;
}

async function logLoop(run, id) {
  let offset = null;
  while (run === logRun && state.selected === id && state.tab === "logs" && visible()) {
    try {
      const chunk = await invoke("log_tail", { id, offset });
      if (run !== logRun) return;
      setProblem("logProblem", null);
      if (offset === null || chunk.reset) {
        state.logLines = chunk.lines || [];
        state.logDirty = true;
        renderLog();
      } else if ((chunk.lines || []).length) {
        state.logLines = state.logLines.concat(chunk.lines);
        renderLog();
      }
      if (state.logLines.length > LOG_KEEP) {
        state.logLines = state.logLines.slice(state.logLines.length - LOG_KEEP);
      }
      offset = chunk.offset;
    } catch (error) {
      if (run !== logRun) return;
      setProblem("logProblem", error);
    }
    await sleep(LOG_POLL_MS);
  }
}

async function copyLog() {
  setProblem("logProblem", null);
  try {
    await navigator.clipboard.writeText(visibleLogLines().map((line) => line.raw).join("\n"));
  } catch (error) {
    setProblem("logProblem", "Could not copy: " + errorText(error));
  }
}

// ---- Wiring -------------------------------------------------------------------------------

function wire() {
  for (const name of TABS) {
    const button = $("tab-" + name);
    button.addEventListener("click", () => showTab(name, false));
    button.addEventListener("keydown", onTabKey);
  }

  $("addFolder").addEventListener("click", async () => {
    const id = await call("addProblem", "add_folder");
    await refresh();
    if (id && state.agents.some((agent) => agent.id === id)) select(id, true);
  });

  $("startAtLogon").addEventListener("change", async (event) => {
    await call("globalProblem", "set_start_at_logon", { on: event.target.checked });
    refresh();
  });
  $("reduceMotion").addEventListener("change", async (event) => {
    await call("globalProblem", "set_global_setting", {
      key: "reduce_motion",
      value: event.target.checked,
    });
    refresh();
  });
  $("menuPeek").addEventListener("change", async (event) => {
    await call("globalProblem", "set_global_setting", {
      key: "menu_peek",
      value: event.target.checked,
    });
    refresh();
  });
  $("switchOver").addEventListener("click", () => call("globalProblem", "switch_over"));
  $("quit").addEventListener("click", () => call("globalProblem", "quit"));

  $("openFolder").addEventListener("click", () => {
    if (state.selected) call("openFolderProblem", "open_folder", { id: state.selected });
  });
  $("openMessenger").addEventListener("click", () => {
    if (state.selected) call("chatProblem", "open_messenger", { id: state.selected });
  });

  $("logLevel").addEventListener("change", renderLog);
  $("logSearch").addEventListener("input", renderLog);
  $("logRaw").addEventListener("change", renderLog);
  $("logCopy").addEventListener("click", copyLog);
  $("logOpen").addEventListener("click", () => {
    if (state.selected) call("logProblem", "open_log", { id: state.selected });
  });

  $("setAutostart").addEventListener("change", (event) =>
    setAgentSetting("autostart", event.target.checked),
  );
  $("setShowPeek").addEventListener("change", (event) =>
    setAgentSetting("show_peek", event.target.checked),
  );
  $("setPeekMessages").addEventListener("change", (event) => {
    const value = Number(event.target.value);
    if (Number.isFinite(value)) setAgentSetting("peek_messages", value);
  });
  $("openConfig").addEventListener("click", () => {
    if (state.selected) call("configProblem", "open_config", { id: state.selected });
  });
  $("remove").addEventListener("click", () => {
    $("remove").hidden = true;
    $("removeConfirm").hidden = false;
    $("removeNo").focus();
  });
  $("removeNo").addEventListener("click", () => {
    hideRemoveConfirm();
    $("remove").focus();
  });
  $("removeYes").addEventListener("click", async () => {
    const id = state.selected;
    hideRemoveConfirm();
    if (!id) return;
    const removed = await call("removeProblem", "remove_agent", { id });
    if (removed !== undefined) {
      state.agents = state.agents.filter((agent) => agent.id !== id);
      select(state.agents.length ? state.agents[0].id : null, false);
    }
    refresh();
  });

  document.addEventListener("visibilitychange", () => {
    if (visible()) {
      refresh();
      startTabLoops();
    } else {
      clearTimeout(refreshTimer);
      refreshTimer = null;
      chatRun += 1;
      logRun += 1;
    }
  });
  window.addEventListener("focus", () => {
    if (visible()) {
      refresh();
      startTabLoops();
    }
  });

  const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
  const reportMotion = () => invoke("report_motion", { reduce: motion.matches }).catch(() => {});
  reportMotion();
  motion.addEventListener("change", reportMotion);
}

wire();
showTab("overview", false);
refresh();
