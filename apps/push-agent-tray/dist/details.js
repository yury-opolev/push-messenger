// The details window: asks the tray for this agent's details every 2 s and shows them.
// Text only (textContent), never HTML.
"use strict";

const REFRESH_MS = 2000;
const FIELDS = ["state", "folder", "session", "codaVersion", "runningTasks", "lastError", "lastExit"];

function text(value) {
  return value === null || value === undefined || value === "" ? "—" : String(value);
}

function time(ms) {
  const date = new Date(ms);
  return Number.isNaN(date.getTime()) ? "" : date.toLocaleString();
}

function render(details) {
  document.getElementById("name").textContent = text(details.name);
  for (const field of FIELDS) {
    document.getElementById(field).textContent = text(details[field]);
  }
  const list = document.getElementById("history");
  const items = (details.history || []).map((entry) => {
    const item = document.createElement("li");
    const at = document.createElement("time");
    at.textContent = time(entry.at);
    const state = document.createElement("span");
    state.textContent = text(entry.state);
    item.append(at, state);
    return item;
  });
  list.replaceChildren(...items);
  document.getElementById("problem").hidden = true;
}

function problem(error) {
  const element = document.getElementById("problem");
  element.textContent = text(error);
  element.hidden = false;
}

async function refresh() {
  try {
    render(await window.__TAURI_INTERNALS__.invoke("details"));
  } catch (error) {
    problem(error);
  }
}

refresh();
setInterval(refresh, REFRESH_MS);
