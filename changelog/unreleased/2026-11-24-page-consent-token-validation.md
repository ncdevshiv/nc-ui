- **Destructive operations now require exact host approval on both agent and page paths.**
  Consent tokens bind the authenticated subject, logical-operation correlation, method,
  capability set and normalized parameter digest; the runtime ledger proves issuance and spends
  destructive tokens once. Altered arguments, another subject, connection, page, command or
  method, forged text and replays are refused without spending the valid operation's token.
- **Removed client-callable consent minting and the page consent boolean.**
  `ncui.consent.mint` no longer exists, and `consent: true` authorizes nothing. The desktop CLI
  installs an `rfd` warning dialog before the first window opens; only explicit Yes approves,
  and approval mints and immediately consumes a token in the same call. Missing approval,
  decline, prompt failure and rate limiting fail closed. Supplied-token failures never fall back
  to a prompt, and oversized operation details are refused rather than truncated.
- **Added security regression evidence.** Dispatch, page-bridge, capability, transport, MCP,
  soak and hosted application suites cover exact prompts, parameter and subject binding,
  cross-connection transfer, replay, non-consumption on refusal, audit correlation and
  pre-window approval installation. `sha2` is used only for canonical operation digests and is
  recorded in ADR-012.
- **Kept comparison ports runnable without weakening page authorization.** The benchmark
  harness explicitly approves only component setup and its result write through a narrow host
  policy; measured IPC calls remain unchanged and every other destructive request is refused.
- **Verified the real Windows confirmation dialog.** UI automation activated the dialog's actual
  Yes and No buttons: Yes ran the destructive command and wrote its marker, while No produced no
  command output.
- **Verified the real Linux confirmation dialog.** Under WSL with a real X11 server and window
  manager, `rfd`'s `zenity` backend received Yes and returned success for `ncui.logs.purge`; Escape
  withheld approval and returned `capability_denied`. Linux hosts need `zenity` for this message
  dialog, and a missing or failing tool fails closed.
