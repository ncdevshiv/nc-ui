# ADR-010: Cross-platform strategy

**Status:** Accepted · **Date:** 2026-10-01

## Context
Linux, Windows, and macOS differ in WebView, GPU stack, input model, path semantics, and process
model. A runtime developed on one platform and tested on one platform will be wrong on the
others in ways that are expensive to discover late.

## Decision
Platform differences are **isolated behind a platform crate**. Shared code is portable by
default, and each platform's divergence is a named module rather than scattered `cfg` attributes.

Path handling, for example, normalises both separator styles because input may arrive with
either. Capability enforcement is identical across platforms, because a weaker platform
implementation is a vulnerability, not a platform difference.

CI runs the build, lint, and test gates on **all three platforms** for every pull request.

## Consequences
- Cross-platform CI is slow and expensive; that cost is the price of discovering breakage before
  release rather than after.
- A feature may be `not_applicable` on a platform, but that must be recorded explicitly in the
  feature registry — silence is not an option.
- Path, filesystem, and text handling get the most adversarial testing, because they are where
  the platforms genuinely differ.

## Implementation status
**Partially implemented.** The toolchain and CI matrix are configured for all three platforms.
Cross-platform path semantics are implemented and tested in `ncui-security`.
