// The Chat tab's entries as DOM nodes: text only (textContent), never HTML, Markdown or
// links. Loaded before app.js; also loaded by the Node tests (tests/chat.test.js).
"use strict";

(function (root) {
  const KIND_LABELS = { step: "step", card: "card", answer: "answer", note: "note" };

  function clockTime(ms) {
    const date = new Date(ms);
    return Number.isNaN(date.getTime()) ? "" : date.toLocaleTimeString();
  }

  function node(doc, tag, className, content) {
    const element = doc.createElement(tag);
    if (className) element.className = className;
    if (content !== undefined) element.textContent = content;
    return element;
  }

  // One <li> per entry, in the order given (the bridge sends them newest last).
  function chatItems(doc, entries) {
    return (entries || []).map((entry) => {
      const item = node(doc, "li", entry.from === "owner" ? "from-owner" : "from-agent");
      const meta = node(doc, "div", "meta");
      meta.append(node(doc, "time", null, clockTime(entry.time)));
      const kind = KIND_LABELS[entry.kind];
      if (kind) {
        const label = entry.kind === "card" && entry.cardState ? kind + " · " + entry.cardState : kind;
        meta.append(node(doc, "span", "kind", label));
      }
      item.append(meta, node(doc, "div", "text", String(entry.text || "")));
      return item;
    });
  }

  root.PushTrayChat = { chatItems };
})(typeof window !== "undefined" ? window : globalThis);
