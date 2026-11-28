/**
 * The NC-UI page bridge.
 *
 * The host injects this script into every page before the page's own scripts run. It owns
 * the request lifecycle: it mints an id for each call, keeps the pending promise, and
 * settles it when the host answers under the same id.
 *
 * The host decides what every action means. This file only carries messages.
 */
(function () {
  if (window.ncui) { return; }

  var protocolVersion = 1;
  var defaultTimeoutMs = 30000;
  var maxPending = 1024;
  var pending = Object.create(null);
  var pendingCount = 0;
  var listeners = Object.create(null);
  var commands = Object.create(null);
  var sequence = 0;

  function isObject(value) {
    return value !== null && typeof value === "object";
  }

  function nextId() {
    sequence += 1;
    var random = "";
    if (window.crypto && typeof window.crypto.getRandomValues === "function") {
      var words = new Uint32Array(2);
      window.crypto.getRandomValues(words);
      random = "-" + words[0].toString(36) + words[1].toString(36);
    }
    return "p" + sequence.toString(36) + "-" + Date.now().toString(36) + random;
  }

  function bridgeError(code, message, retryable) {
    var error = new Error(message);
    error.name = "NcuiError";
    error.code = code;
    error.retryable = retryable === true;
    return error;
  }

  function settle(message) {
    if (!isObject(message)) { return; }
    if (typeof message.id === "string") {
      var entry = pending[message.id];
      if (!entry) { return; }
      delete pending[message.id];
      pendingCount -= 1;
      if (entry.timer !== null) { clearTimeout(entry.timer); }
      if (message.ok) {
        entry.resolve(message.payload === undefined ? null : message.payload);
        return;
      }
      var failure = isObject(message.error)
        ? message.error
        : { code: "bridge_error", message: "the host refused the request", retryable: false };
      var rejected = bridgeError(
        typeof failure.code === "string" ? failure.code : "bridge_error",
        typeof failure.message === "string" ? failure.message : "the host refused the request",
        failure.retryable
      );
      rejected.outcome = typeof message.outcome === "string" ? message.outcome : "";
      entry.reject(rejected);
      return;
    }
    if (typeof message.event === "string") {
      var handlers = listeners[message.event];
      if (!handlers) { return; }
      for (var index = 0; index < handlers.length; index += 1) {
        try {
          handlers[index](message.payload === undefined ? null : message.payload);
        } catch (error) {
          // A page's own listener failure must not stop later listeners.
        }
      }
    }
  }

  function post(request, options) {
    return new Promise(function (resolve, reject) {
      if (pendingCount >= maxPending) {
        reject(bridgeError("backpressure", "too many requests are awaiting a host response", true));
        return;
      }
      var timeoutMs = defaultTimeoutMs;
      if (isObject(options) && typeof options.timeoutMs === "number") {
        timeoutMs = options.timeoutMs;
      }
      var id = nextId();
      var timer = null;
      if (timeoutMs > 0) {
        timer = setTimeout(function () {
          if (pending[id]) {
            delete pending[id];
            pendingCount -= 1;
            reject(bridgeError("timeout", "the host did not answer within " + timeoutMs + " ms", true));
          }
        }, timeoutMs);
      }
      pending[id] = { resolve: resolve, reject: reject, timer: timer };
      pendingCount += 1;
      var message = { id: id, action: request.action, payload: request.payload === undefined ? null : request.payload };
      if (request.name !== undefined) { message.name = request.name; }
      // Supersede keys travel as transport metadata: a keyed command whose
      // successor is already queued is answered "superseded" without executing,
      // which is what makes a scrub a latest-wins stream instead of a backlog.
      // Only the caller knows which of its commands form such a stream, so the
      // key is per-call, never per-registration.
      if (isObject(options) && typeof options.supersede === "string" && options.supersede.length > 0) {
        message.supersede = options.supersede;
      }
      for (var key in request.extra) {
        if (Object.prototype.hasOwnProperty.call(request.extra, key)) {
          message[key] = request.extra[key];
        }
      }
      try {
        window.ipc.postMessage(JSON.stringify(message));
      } catch (error) {
        if (timer !== null) { clearTimeout(timer); }
        delete pending[id];
        pendingCount -= 1;
        reject(bridgeError("transport_failed", "the page could not reach the host", false));
      }
    });
  }

  function on(event, handler) {
    if (typeof event !== "string" || typeof handler !== "function") {
      return function () {};
    }
    if (!listeners[event]) { listeners[event] = []; }
    listeners[event].push(handler);
    return function () {
      var handlers = listeners[event];
      if (!handlers) { return; }
      var index = handlers.indexOf(handler);
      if (index >= 0) { handlers.splice(index, 1); }
      if (handlers.length === 0) { delete listeners[event]; }
    };
  }

  // Runs a command the page registered and posts the answer back under the reply id the
  // runtime minted. The runtime started the call and is holding the page's original invoke
  // open, so this is the second half of a round trip: the answer travels as its own message
  // rather than as a return value, because the host cannot read one back out of the script
  // it evaluates. A handler that is missing, or that throws, is reported as a failure rather
  // than leaving the runtime's call to time out.
  function runCommand(replyId, name, payload) {
    var handler = commands[name];
    var answer;
    if (typeof handler !== "function") {
      answer = {
        id: replyId,
        action: "command_result",
        ok: false,
        error: {
          code: "not_registered",
          message: "the page has not registered a handler for `" + name + "`",
          retryable: false
        }
      };
    } else {
      try {
        var result = handler(payload === undefined ? null : payload);
        answer = {
          id: replyId,
          action: "command_result",
          ok: true,
          payload: result === undefined ? null : result
        };
      } catch (error) {
        answer = {
          id: replyId,
          action: "command_result",
          ok: false,
          error: {
            code: "handler_failed",
            message: error && error.message ? String(error.message) : "the page's handler threw",
            retryable: false
          }
        };
      }
    }
    try {
      window.ipc.postMessage(JSON.stringify(answer));
    } catch (transportError) {
      // The runtime is holding the call and will report it as unanswered; there is no second
      // channel to fall back to.
    }
  }

  Object.defineProperty(window, "ncui", {
    value: Object.freeze({
      protocolVersion: protocolVersion,
      registerCommand: function (name, handler) {
        if (typeof name !== "string" || name.length === 0) { return false; }
        if (typeof handler !== "function") { return false; }
        commands[name] = handler;
        return true;
      },
      invoke: function (action, payload, options) {
        return post(
          { action: "perform", name: String(action), payload: payload, extra: {} },
          options
        );
      },
      describe: function (options) {
        return post({ action: "describe", extra: {} }, options);
      },
      probeNavigation: function (url, options) {
        return post({ action: "probe_navigation", extra: { url: String(url) } }, options);
      },
      on: on,
      once: function (event, handler) {
        var off = null;
        off = on(event, function (payload) {
          off();
          handler(payload);
        });
        return off;
      },
      __dispatch: settle,
      __runCommand: runCommand,
      // Receives one CDP command from the host (`ncui.cdp.send`). The bridge has
      // no debugging transport of its own: it records the command in a bounded
      // ring a page can read, and re-dispatches it to listeners registered with
      // `ncui.on("ncui:cdp", ...)`, so a page that wants a relay implements one
      // where its own debugging endpoint lives.
      __cdpSend: function (command) {
        if (!isObject(command)) { return false; }
        var log = window.__ncuiCdpLog || (window.__ncuiCdpLog = []);
        if (log.length >= 64) { log.shift(); }
        log.push(command);
        settle({ event: "ncui:cdp", payload: command });
        return true;
      }
    })
  });
})();
