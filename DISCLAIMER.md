# Disclaimer

## 1. This project is almost entirely AI-written

**Nearly all of the code in this repository was written by artificial intelligence,
and nearly all of the testing was designed and executed by artificial
intelligence as well.**

That includes, but is not limited to: the runtime crates, the CLI, the GPU
renderer, the WebView host, the security layer, the test suites, the fuzz
targets, the benchmark harness, the CI workflows, the build scripts, and the
documentation you are reading.

Humans set the direction, reviewed output, and made the decisions that mattered.
Humans did not hand-write the bulk of the implementation. Where a file or a
component is an exception — third-party code, upstream ports under
`comparison/`, or generated assets — that is stated where it appears.

We are stating this plainly and up front rather than burying it, because you
deserve to know what you are evaluating before you read a single line of it.

## 2. What "written and tested by AI" does and does not mean

**It does mean:**

- The code compiles and passes the project's own gates (`make gate`) as recorded
  in CI and in the changelog fragments.
- Every claimed feature is expected to cite the files and tests behind it in
  `protocols/feature-registry.json`; an entry that cites a nonexistent file is
  rejected.
- The tests were real tests that actually ran — not assertions added after the
  fact to make a number look good.

**It does not mean:**

- **That the code is correct.** AI systems produce plausible-looking code,
  including plausible-looking code that is subtly wrong. The gates catch what
  the gates measure. They do not catch what nobody thought to measure.
- **That the code is secure.** A capability model, an audit trail, and a
  path-containment check that pass their tests are still untrusted until an
  independent human reviews them and fails to break them.
- **That the code is production-ready.** It is not. Treat NC-UI as an
  experimental runtime under active development, not as a dependency you ship
  to users today.
- **That the documentation is accurate.** Documentation is the easiest thing for
  an AI to write convincingly and the easiest thing to leave behind when the
  code moves. Known discrepancies are tracked in `docs/known-gaps.md`.

The project's own standard applies here more than anywhere: **an unverified
claim is worse than an absent feature.** Assume a claim is unverified until you
have reproduced it.

## 3. No warranty

This software is provided under the Apache License 2.0, **on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND**, either express or implied,
including, without limitation, any warranties or conditions of TITLE,
NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A PARTICULAR PURPOSE.

You are solely responsible for determining the appropriateness of using or
redistributing this work. Do not run it with privileges, data, or network access
you would not hand to an unfamiliar experimental program.

See [LICENSE](LICENSE) for the binding terms. This document is a plain-language
explanation of intent and does not modify, extend, or replace the licence.

## 4. We are open to suggestions, enhancements, features, and improvements

This is a genuinely open repository, and the fastest way to make it better is
for people who actually build things to say what is missing or wrong. All of the
following are welcome, from anyone, at any level of experience:

| You have | Do this |
| --- | --- |
| A bug report | Open an issue with the smallest reproduction you can manage |
| A correction to a claim in the README or the docs | Open an issue or a pull request citing the file and line |
| A feature request | Open an issue describing the problem, not only the solution |
| An enhancement to existing behaviour | Open a pull request that includes the test that proves it |
| A security concern | Report it privately first — see below |
| A question about the architecture | Open a discussion or an issue referencing the relevant ADR in `docs/adr/` |
| A port, benchmark, or comparison against another framework | Open an issue before writing it, so the harness stays consistent |

**Pull requests should carry the evidence, not just the patch.** The repository
has gates: file-size ceilings, folder-shape limits, a hygiene scan, a changelog
fragment flow, and a feature registry that refuses uncited claims. A change that
is correct but fails a gate is not mergeable; a change that passes a gate but
changes nothing measurable is not worth merging.

### Reporting a security issue

Please do **not** open a public issue for a suspected vulnerability. Describe
the impact and a reproduction privately to the maintainers, and allow time for a
fix before disclosure. Given the AI-authored nature of this codebase, security
reports are treated as especially valuable: they are the primary way the gap
between "passes tests" and "is actually safe" gets closed.

## 5. Attribution and provenance

- Licence: **Apache License 2.0** — [LICENSE](LICENSE)
- Attribution: [NOTICE](NOTICE)
- Contributors: **NC-UI Contributors**
- Repository: <https://github.com/ncdevshiv/nc-ui>

If you use this project, keep the `LICENSE` and `NOTICE` files with it. If you
redistribute a modified version, say prominently that you changed it — and be
equally honest about which parts, if any, were written by a machine.

---

*If you find that a claim in this repository overstates what the code does, that
is a bug, and it is the most useful bug you can report.*
