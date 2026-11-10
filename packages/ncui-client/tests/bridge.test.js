import { readFile } from "node:fs/promises";
import { createContext, runInContext } from "node:vm";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import test from "node:test";
import assert from "node:assert/strict";

const here = dirname(fileURLToPath(import.meta.url));
const bridgeSource = await readFile(join(here, "..", "src", "bridge.js"), "utf8");

/**
 * Runs the bridge in a context that behaves like a page.
 *
 * The context records what the page posts and lets the test answer each request, which is
 * the same relationship the host has with a real page: the page posts, the host answers.
 */
function createPage() {
  const sent = [];
  const window = {
    ipc: {
      postMessage(body) {
        sent.push(JSON.parse(body));
      }
    },
    crypto: {
      getRandomValues(words) {
        for (let index = 0; index < words.length; index += 1) {
          words[index] = index + 1;
        }
        return words;
      }
    }
  };
  const context = createContext({
    window,
    setTimeout,
    clearTimeout,
    Date,
    Promise,
    console
  });
  runInContext(bridgeSource, context);
  return {
    ncui: window.ncui,
    context,
    sent,
    answer(id, payload) {
      window.ncui.__dispatch({ id, ok: true, payload });
    },
    fail(id, code, message) {
      window.ncui.__dispatch({ id, ok: false, error: { code, message } });
    }
  };
}

test("a call posts an id, an action and its payload", async () => {
  const page = createPage();
  const pending = page.ncui.invoke("add_numbers", { a: 2, b: 3 });

  assert.equal(page.sent.length, 1);
  const request = page.sent[0];
  assert.equal(request.action, "perform");
  assert.equal(request.name, "add_numbers");
  assert.deepEqual(request.payload, { a: 2, b: 3 });
  assert.equal(typeof request.id, "string");
  assert.ok(request.id.length > 0);

  page.answer(request.id, { sum: 5 });
  assert.deepEqual(await pending, { sum: 5 });
});

test("two calls in flight are answered by their own id, in any order", async () => {
  const page = createPage();
  const first = page.ncui.invoke("echo", { n: 1 });
  const second = page.ncui.invoke("echo", { n: 2 });

  const [firstRequest, secondRequest] = page.sent;
  assert.notEqual(firstRequest.id, secondRequest.id);

  page.answer(secondRequest.id, { n: 2 });
  page.answer(firstRequest.id, { n: 1 });

  assert.deepEqual(await first, { n: 1 });
  assert.deepEqual(await second, { n: 2 });
});

test("a refusal rejects with the host's code", async () => {
  const page = createPage();
  const pending = page.ncui.invoke("write_note");
  page.fail(page.sent[0].id, "capability_denied", "the page may not write");

  await assert.rejects(pending, (error) => {
    assert.equal(error.name, "NcuiError");
    assert.equal(error.code, "capability_denied");
    assert.equal(error.message, "the page may not write");
    return true;
  });
});

test("a call that is never answered fails on its own timeout", async () => {
  const page = createPage();
  await assert.rejects(page.ncui.invoke("slow", null, { timeoutMs: 20 }), (error) => {
    assert.equal(error.code, "timeout");
    assert.equal(error.retryable, true);
    return true;
  });
});

test("a late answer for a timed-out call changes nothing", async () => {
  const page = createPage();
  const pending = page.ncui.invoke("slow", null, { timeoutMs: 20 });
  const id = page.sent[0].id;
  await assert.rejects(pending);
  page.answer(id, { late: true });
});

test("describe asks the host what the page may do", async () => {
  const page = createPage();
  const pending = page.ncui.describe();
  assert.equal(page.sent[0].action, "describe");
  page.answer(page.sent[0].id, { protocol_version: 1, actions: [] });
  assert.deepEqual(await pending, { protocol_version: 1, actions: [] });
});

test("a navigation probe carries the url", async () => {
  const page = createPage();
  const pending = page.ncui.probeNavigation("https://app.example.com/next");
  assert.equal(page.sent[0].action, "probe_navigation");
  assert.equal(page.sent[0].url, "https://app.example.com/next");
  page.answer(page.sent[0].id, { ok: true });
  assert.deepEqual(await pending, { ok: true });
});

test("an event reaches its listeners", async () => {
  const page = createPage();
  const seen = [];
  page.ncui.on("tick", (payload) => seen.push(payload));

  page.ncui.__dispatch({ event: "tick", payload: 1 });
  page.ncui.__dispatch({ event: "tick", payload: 2 });

  assert.deepEqual(seen, [1, 2]);
});

test("a listener that throws does not stop the later listeners", async () => {
  const page = createPage();
  const seen = [];
  page.ncui.on("tick", () => {
    throw new Error("the page's own failure");
  });
  page.ncui.on("tick", (payload) => seen.push(payload));

  page.ncui.__dispatch({ event: "tick", payload: 7 });

  assert.deepEqual(seen, [7]);
});

test("once fires exactly once and can be removed", async () => {
  const page = createPage();
  const seen = [];
  page.ncui.once("tick", (payload) => seen.push(payload));

  page.ncui.__dispatch({ event: "tick", payload: 1 });
  page.ncui.__dispatch({ event: "tick", payload: 2 });

  assert.deepEqual(seen, [1]);

  const off = page.ncui.on("tick", () => seen.push("never"));
  off();
  page.ncui.__dispatch({ event: "tick", payload: 3 });
  assert.deepEqual(seen, [1]);
});

test("the bridge refuses to queue more work than it can carry", async () => {
  const page = createPage();
  const pending = [];
  for (let index = 0; index < 1024; index += 1) {
    const call = page.ncui.invoke("echo", { index }, { timeoutMs: 50 });
    // The unanswered calls must not surface as unhandled rejections when their own
    // timeout fires; the test only cares about the refusal and the first answer.
    call.catch(() => {});
    pending.push(call);
  }
  assert.equal(page.sent.length, 1024);

  await assert.rejects(page.ncui.invoke("echo", { index: 1024 }), (error) => {
    assert.equal(error.code, "backpressure");
    return true;
  });

  page.answer(page.sent[0].id, { index: 0 });
  assert.deepEqual(await pending[0], { index: 0 });
});

test("the bridge is installed once and stays frozen", () => {
  const page = createPage();
  const first = page.ncui;
  runInContext(bridgeSource, page.context);
  assert.equal(page.ncui, first, "a second injection must not replace the bridge");
  assert.throws(() => {
    page.ncui.invoke = null;
  }, TypeError);
});
