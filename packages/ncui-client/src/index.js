/**
 * The NC-UI client API.
 *
 * The host injects the bridge into every page; this module is the typed surface an
 * application imports. It adds names, error types and availability checks, and it
 * delegates the transport to the bridge rather than reimplementing it.
 */

/** A failure reported by the host. */
export class NcuiError extends Error {
  /**
   * @param {string} message the host's message
   * @param {string} code the host's stable machine-readable code
   * @param {boolean} retryable whether the same call could succeed later
   */
  constructor(message, code, retryable) {
    super(message);
    this.name = "NcuiError";
    this.code = code;
    this.retryable = retryable === true;
  }
}

function bridge() {
  const found = globalThis.ncui;
  if (!found) {
    throw new NcuiError(
      "the NC-UI bridge is not present; this page is not running inside an NC-UI window",
      "bridge_unavailable",
      false
    );
  }
  return found;
}

function toNcuiError(error) {
  if (error instanceof NcuiError) {
    return error;
  }
  if (error && typeof error === "object" && typeof error.code === "string") {
    const wrapped = new NcuiError(
      typeof error.message === "string" ? error.message : String(error),
      error.code,
      error.retryable === true
    );
    if (typeof error.outcome === "string") {
      wrapped.outcome = error.outcome;
    }
    return wrapped;
  }
  return new NcuiError(
    error instanceof Error ? error.message : String(error),
    "unknown_error",
    false
  );
}

/**
 * Whether the page is running inside an NC-UI window.
 *
 * @returns {boolean} true when the host injected the bridge
 */
export function isAvailable() {
  return Boolean(globalThis.ncui);
}

/**
 * The bridge protocol version the host speaks.
 *
 * @returns {number} the protocol version
 */
export function protocolVersion() {
  return bridge().protocolVersion;
}

/**
 * Calls a native command.
 *
 * @param {string} action the command name
 * @param {object|null} [payload] the command's arguments
 * @param {{timeoutMs?: number}} [options] call options
 * @returns {Promise<unknown>} the command's result
 */
export async function invoke(action, payload, options) {
  try {
    return await bridge().invoke(action, payload, options);
  } catch (error) {
    throw toNcuiError(error);
  }
}

/**
 * Describes the runtime, the commands available to this page, and the page's grant.
 *
 * @param {{timeoutMs?: number}} [options] call options
 * @returns {Promise<object>} the description
 */
export async function describe(options) {
  try {
    return await bridge().describe(options);
  } catch (error) {
    throw toNcuiError(error);
  }
}

/**
 * Asks whether a navigation would be permitted.
 *
 * @param {string} url the target url
 * @param {{timeoutMs?: number}} [options] call options
 * @returns {Promise<object>} the verdict
 */
export async function probeNavigation(url, options) {
  try {
    return await bridge().probeNavigation(url, options);
  } catch (error) {
    throw toNcuiError(error);
  }
}

/**
 * Subscribes to a host event.
 *
 * @param {string} event the event name
 * @param {(payload: unknown) => void} handler the listener
 * @returns {() => void} a function that removes the listener
 */
export function on(event, handler) {
  return bridge().on(event, handler);
}

/**
 * Subscribes to the next occurrence of a host event.
 *
 * @param {string} event the event name
 * @param {(payload: unknown) => void} handler the listener
 * @returns {() => void} a function that removes the listener
 */
export function once(event, handler) {
  return bridge().once(event, handler);
}
