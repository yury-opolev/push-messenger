// The Chat tab renders peek entries as text only. Run: node --test apps/push-agent-tray/tests/chat.test.js
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const path = require("node:path");

require(path.join(__dirname, "..", "dist", "chat.js"));
const { chatItems } = globalThis.PushTrayChat;

// A minimal document: elements record textContent and children, and fail on any HTML API.
function fakeDocument() {
  return {
    createElement(tag) {
      const element = {
        tag,
        className: "",
        textContent: "",
        children: [],
        append(...nodes) {
          this.children.push(...nodes);
        },
        insertAdjacentHTML() {
          throw new Error("insertAdjacentHTML used");
        },
      };
      for (const api of ["innerHTML", "outerHTML"]) {
        Object.defineProperty(element, api, {
          set() {
            throw new Error(api + " used");
          },
          get() {
            return "";
          },
        });
      }
      return element;
    },
  };
}

function textOf(item) {
  return item.children[1].textContent;
}

test("entries keep their order and render as text", () => {
  const entries = [
    { time: 1, from: "owner", kind: "text", text: "<b>run</b> the [tests](https://x.test)" },
    { time: 2, from: "agent", kind: "step", text: "✓ run_command dotnet test" },
    { time: 3, from: "agent", kind: "reply", text: "**Tests pass**\n<img src=x onerror=alert(1)>" },
  ];
  const items = chatItems(fakeDocument(), entries);
  assert.strictEqual(items.length, 3);
  assert.strictEqual(textOf(items[0]), "<b>run</b> the [tests](https://x.test)");
  assert.strictEqual(textOf(items[1]), "✓ run_command dotnet test");
  assert.strictEqual(textOf(items[2]), "**Tests pass**\n<img src=x onerror=alert(1)>");
  assert.deepStrictEqual(
    items.map((item) => item.className),
    ["from-owner", "from-agent", "from-agent"],
  );
  for (const item of items) {
    assert.strictEqual(item.tag, "li");
    assert.strictEqual(item.children[1].children.length, 0, "the text has no child nodes");
  }
});

test("kinds and card states are labelled, plain replies are not", () => {
  const items = chatItems(fakeDocument(), [
    { time: 1, from: "agent", kind: "card", text: "Allow x? [Approve] [Reject]", cardState: "open" },
    { time: 2, from: "owner", kind: "answer", text: "Approve" },
    { time: 3, from: "agent", kind: "reply", text: "done" },
  ]);
  const labels = items.map((item) =>
    item.children[0].children.slice(1).map((child) => child.textContent),
  );
  assert.deepStrictEqual(labels, [["card · open"], ["answer"], []]);
  assert.strictEqual(items[0].children[0].children[0].tag, "time");
});

test("no entries render nothing", () => {
  assert.deepStrictEqual(chatItems(fakeDocument(), []), []);
  assert.deepStrictEqual(chatItems(fakeDocument(), undefined), []);
});
