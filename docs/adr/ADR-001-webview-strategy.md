# ADR-001: WebView strategy

**Status:** Accepted · **Date:** 2026-10-01

## Context
NC-UI targets HTML/CSS/JavaScript and the entire framework ecosystem built on it. Building a
browser engine is a decade of work, and a worse one than the engines that already exist.

## Decision
NC-UI embeds a **system WebView** through a mature embedding layer. The WebView owns HTML
parsing, CSS layout, JavaScript execution, networking, and the accessibility tree. NC-UI owns
the application runtime around it: windows, native capabilities, GPU surfaces, IPC,
scheduling, security, and the agent interface.

NC-UI does not patch, fork, or inject into the browser engine. The seam is deliberately a
narrow, documented boundary so that swapping the embedding layer later is a contained change.

## Consequences
- We inherit the WebView's security posture, including its patch cadence, which is a genuine
  advantage over maintaining our own.
- We do not inherit its rendering performance. Anything performance-critical is moved off the
  DOM onto GPU islands (ADR-002) rather than being optimised inside the WebView.
- A WebView crash is a real failure mode that must be detected, reported, and recovered from;
  it is in the fault-injection matrix, not treated as impossible.
- Engine differences between platforms (WebView2, WebKitGTK, WKWebView) are a permanent source
  of cross-platform test surface.

## Implementation status
**Host policy implemented; the native binding is not built.**

`ncui-webview` holds the half ADR-001 assigns to NC-UI: window lifecycle including the crash
path, capability isolation for page content, the navigation rules, and the IPC seam. All of
it is pure logic over an injected `WebViewBackend`, so it is testable without a display — 51
tests, and two benchmark scenarios measuring the per-navigation and per-message costs.

The binding itself is deliberately not a dependency (ADR-012). The WebView2 runtime *is*
installed on this machine and a window can be created and shown in this session, so the
obstacle is the dependency's size and its unverifiability in CI rather than the hardware.
`docs/known-gaps.md` records the position.
