/** Options every bridge call accepts. */
export interface CallOptions {
  /**
   * How long to wait for the host, in milliseconds. `0` waits forever.
   * The default is 30000.
   */
  timeoutMs?: number;
}

/** A failure reported by the host. */
export declare class NcuiError extends Error {
  constructor(message: string, code: string, retryable?: boolean);
  /** The host's stable machine-readable code. */
  readonly code: string;
  /** Whether the same call could succeed later. */
  readonly retryable: boolean;
  /** The host's machine-readable outcome, when it sent one. */
  readonly outcome?: string;
}

/** One command the page may call. */
export interface ActionDescription {
  /** The command name. */
  name: string;
  /** The capabilities the page must hold to call it. */
  requires: string[];
  /** What the command does. */
  summary: string;
}

/** What `describe` reports. */
export interface RuntimeDescription {
  /** The bridge protocol version the host speaks. */
  protocol_version: number;
  /** The capabilities this page holds. */
  capabilities: string[];
  /** The commands this page may call. */
  actions: ActionDescription[];
}

/** Whether the page is running inside an NC-UI window. */
export declare function isAvailable(): boolean;

/** The bridge protocol version the host speaks. */
export declare function protocolVersion(): number;

/** Calls a native command. */
export declare function invoke(
  action: string,
  payload?: object | null,
  options?: CallOptions
): Promise<unknown>;

/** Describes the runtime, the commands available to this page, and the page's grant. */
export declare function describe(options?: CallOptions): Promise<RuntimeDescription>;

/** Asks whether a navigation would be permitted. */
export declare function probeNavigation(
  url: string,
  options?: CallOptions
): Promise<{ ok: boolean; outcome: string }>;

/** Subscribes to a host event. */
export declare function on(
  event: string,
  handler: (payload: unknown) => void
): () => void;

/** Subscribes to the next occurrence of a host event. */
export declare function once(
  event: string,
  handler: (payload: unknown) => void
): () => void;
