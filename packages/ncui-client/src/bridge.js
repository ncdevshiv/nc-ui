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

  Object.defineProperty(window, "ncui", {
    value: Object.freeze({
      protocolVersion: protocolVersion,
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
      __dispatch: settle
    })
  });
})();
