# Principles and Guidelines

This document states the minimum principles, guidelines, and rules an agent must
satisfy when changing this codebase. It is generalized: it names no specific
file, package, symbol, or hardcoded value. Where a principle conflicts with a
local convention, the local convention wins only when it is documented and
reviewed; otherwise this document wins — except that a mechanical check's policy
owns the question it decides mechanically.

Every change is judged by whether it keeps correctness explicit, keeps one
source of truth, extends through registered seams, keeps the change face equal
to the review face, and carries evidence. A principle is a requirement, not a
suggestion.

---

## How to use this document

- **Each rule states what must hold.** Where a rule is judgeable, the observable
  condition a reviewer checks is part of the rule. Where a rule is not judgeable
  by inspection, it states the evidence that settles it.
- **A rule names its exception channel or has none.** A rule that admits an
  exception names the domain owner of that surface who approves it, records the
  reason on the change, and is never self-approved by the author. A rule that
  names no approver has no exception channel.
- **Apply the rules per artifact.** Read the rules for the surface you are
  changing, not one uniform checklist. A rule filed under one surface does not
  constrain another, except where the section states that it binds every
  surface.
- **One home per fact.** An obligation has one home. A specialized surface
  states the obligation only when it adds mechanism the general rule does not
  carry; otherwise it points at the home.
- **Prefer the general rule, then the mechanism.** A general statement that
  loses the mechanism is decoration; a specific statement that loses the
  general rule is a trap. A rule states both.

### Terms

- A **capability** is a swappable behavior with a definition, a provider, and a
  consumer.
- A **feature** is one capability as a caller or a user meets it.
- A **domain** is a group of related capabilities.
- A **package** ships one feature, or one domain's share of it. A **module** is
  a file inside a package.
- A **unit** is one registered entry in a registry: a projection, a stored
  record type, a control, a slot.
- A **surface** is one section of this document: Backend, Frontend, Database, or
  Cross-cutting.
- A **seat** is one named slot on the inject face of a registration, bound to a
  hook the view calls.
- A **durable record** is a fact appended to a log that survives a reload. A
  **live event** reports work in flight and is not durable.
- A **projection** folds committed records into a derived state that readers
  read instead of recomputing.
- A **cold read** is a read of the original record set rather than of a
  projection. It is the slow path.
- An **adjacent migration** is one step from one stored version to the next.
- **Interface availability** is the presence of a usable value, as distinct
  from the correctness of that value.

---

## Backend

### Service and capability design

- A capability seam has three roles: a definition, a provider, and a consumer.
  A package may combine roles, but one role alone is not a seam. Adding a
  capability means designing all three. Split a seam only when its roles
  evolve independently.
- A business capability lives in its own package and attaches through a
  registration surface: a registry, an injection slot, a delegating chain, or
  an event subscription. The center layer holds registry structure and disposal
  lifecycle only; the entries are written by the packages that own them, and it
  never hardcodes a central table of business rows.
- Pure infrastructure — logging, transport, storage — is the exception: it
  does not attach through a business registration surface. It still plugs in
  through the interface it implements.
- A new branch in a center layer must explain why it cannot live in the owning
  package. That explanation is reviewed as its own question.
- New behavior attaches to a documented extension point. Changing a core
  driving loop requires updating the project's architecture documentation in the
  same change.
- No part of the system is privileged. A change that patches a core where a
  component mounted beside the others would do is the defect; mounting it keeps
  every component replaceable by configuration.
- Services are split by business boundary, not by caller count. A capability
  with a single caller still owns its service. A service whose methods all
  serve one business boundary stays whole, and its private helpers belong on
  it.
- A public service method with exactly one internal caller is a smell: pass a
  private capability closure instead.
- Design a service definition against all current consumers. Keep
  consumer-specific behavior — schema, loader, interface, transport, provider —
  in the consumer or provider, and never let one consumer dictate the
  contract.
- A published interface carries no stability promise, so a change to it updates
  every consumer in the same change. A consumer that cannot be updated in the
  same change is a reason to add a new entry rather than change the existing
  one.
- A plugin uses only the dependencies its own declaration names. The container
  it was handed is not a wider permission to reach for anything else.
- Contributions are registered as effects, and a registration API returns the
  disposer that unwinds the contribution. A contribution into another owner's
  extension point waits for that declaration, installs when it appears, is
  removed when the declaration collapses, and is reinstalled after a
  redeclaration. A bare registration into an undeclared point is an error.
- One feature is one package. A package with several domains splits along the
  line where its code could later become separate packages; sibling domains
  never import each other; one assembly point composes them. Registration
  happens in the plugin's apply phase, never as a module-level side effect.
- Tie every abstraction, state machine, option, defensive copy, and
  compatibility path to a current contract or a production consumer, and keep
  the behavior in the module that owns it. Remove what has neither.
- Exposing an option does not by itself justify a default, a public operation
  set, a format, or an imported external concept. Each needs current-consumer
  evidence or relevant prior art; otherwise require an explicit value or defer
  the choice.
- Prefer maintained dependencies over hand-rolling when they genuinely delete
  owned code and tests.
- Enforce a decision in the operation that makes it. Omitting a field,
  filtering text, adding a facade, adding a wrapper, or relying on listener
  order is not enforcement when a direct or alternate caller can bypass it.
  Prove the denial through the executor, not through the façade.
- Model-facing contracts are written from the model's perspective. Prompts,
  tool schemas, results, and diagnostics contain only task-relevant concepts,
  never interface, transport, or implementation vocabulary. Pin stable
  model-visible text verbatim; pin dynamic behavior through recorded or
  end-to-end coverage.
- A code relocation — including a package move or an export-path change — keeps
  externally observable behavior equal: error codes, diagnostics, data-flow
  order, and interface timing, compared item by item. Moving code is preferred
  over deleting and rewriting it. A carrier change and a behavior change are
  separate changes, and a defect found during a move is fixed as its own change.
- A plugin reads an optional service through the strict lookup accessor.
  Reserve the property form for declared injections, because the property
  proxy's resolution is topology-sensitive.

### API and remote boundaries

- A new remote business capability gets its own service. An endpoint that
  changes owners moves to the new owner; the object it leaves keeps no copy
  and no forwarding stub.
- The remote marker goes on the concrete method that performs the operation,
  never on a wrapper that forwards. Judge it by the decorated method's body: a
  body that only delegates means the marker is on the wrong method.
- Client-facing types come from the single generated source. Never hand-write
  a second copy of a generated type.
- The wire payload is JSON, and binary content is expressed field-level. There
  is no output-side decode inside a remote method; the codec and the gateway
  validate the result.
- A remote method's signature uses only what the transport supports:
  arguments resolved to host objects, a trailing cancellation signal, and
  streams declared as streams. A parameter the transport cannot resolve is a
  contract error, not a client-side convenience.
- Existing method and field names are additive. A new semantic gets a new
  entry; a rename is its own change, states its benefit and the impact on
  existing callers first, and needs the domain owner's confirmation. The
  author never self-approves.
- Every call path names its peer explicitly. The peer is a field of the
  invocation, and a path that resolves a peer from a default, from ambient
  state, or from a parameter the caller controls is the defect.
- The caller identity is read only from the invocation context, never from
  arguments. A user-message source built in a remote method records its
  principal through the principal helper and passes the built source down.
- Access control is enforced at the boundary and enumerates every resource the
  operation touches. A resource it omits is never checked. A method returning
  several resources filters each item through the access check.
- A method without the remote marker stays an ordinary in-process service
  method.
- Data that carries its own protocol — streams, pagination, incremental
  delivery, projections, entity substreams — keeps its own registration model.
  It is not exposed as an ordinary remote method on the service that owns the
  capability.
- A call identifier is minted by the initiator and echoed unchanged by the
  responder, and minting lives in the transport, not in either endpoint's
  business code.
- A service edge, a module-graph edge, and a package-metadata edge are three
  different declarations and are not interchangeable. A service edge waits at
  runtime and may be satisfied by any provider; a module edge is materialized
  synchronously, has one supplier, and rejects cycles; a metadata edge orders
  nothing.
- A new storage backend implements the persistence interface over the shared
  handle scaffolding rather than forking it.

### Events, extension points, and the request lifecycle

- Choose the event channel by what the fact is. A fact that must survive a
  reload is a durable record; an observation of work in flight is a live event;
  policy or an adapter for a capability is published on the capability's own
  channel without importing the driving loop.
- Typed events are declared through mergeable map types so other modules can
  add events. Document each event's delivery mode and its payload parameters.
  An event is required on read unless its envelope explicitly marks it
  ignorable.
- A listener on a delegating chain must delegate to the next participant.
  Returning without delegating short-circuits the chain and silently disables
  every later listener. A non-delegating channel has no delegate and must not
  offer one.
- Switch on discriminant tags. A closed union ends in an exhaustive check; a
  merge-extensible union falls through a documented default.
- A cancelled operation commits nothing: no partial prompt, no partial
  durable state, no half-applied turn. The commit point comes after every
  asynchronous resolution that can change what would be committed.
- A step that fails after issuing a request still records the missing result,
  so the durable log stays well-formed and replayable. Failure is recorded as
  an outcome, never as an absence.
- Initialization is transactional: a failure unwinds everything the component
  registered. Unloading releases resources through an ordered teardown, not by
  abandonment.
- A prepared request is frozen. A change to what would be sent starts a new
  series rather than mutating the prepared one, and a retry does not repeat
  assembly or re-admission.
- Only settled outcomes are durable. A settled attempt — failed, retried,
  cancelled, or errored — is retained for inspection but contributes nothing to
  model history. Live progress is an ephemeral channel.
- Model-visible means logged. Anything that reaches a model request must be
  reconstructable from the durable log; a new model-visible input requires a
  durable event.
- Presentation-only data never enters the durable record. A rendered result is
  derived from the recorded facts and the persisted result metadata, so the
  record alone can rebuild it, and a tool's presentation is designed together
  with the tool.
- A durable record is published by the component that owns its invariants. A
  hand-built copy of that record is not persisted.
- A component that changes message content registers a pure projection of that
  change. A reader that detaches from the live stream supplies the same
  definitions explicitly, so every reader sees the same message.
- A component that requires a service requires it during activation and fails
  explicitly when the registry or the required key is absent. It never
  silently substitutes a default for a missing host value.
- The projection is the only read and compute path for derived state. A
  component that fetches session data itself, or a computation a reader
  triggers on demand, is the violation.
- A registered projection unit carries a version that starts at one and is
  bumped whenever its serialized fields or its fold semantics change,
  compatible or not. Registering the same key twice with different versions
  fails loudly instead of merging.
- A derived-state unit declares the full set of facts it produces, and it
  computes them. Narrowing that declared set changes observable semantics and
  requires the owning domain's confirmation, never the implementer's own
  decision. Computing the declared set incrementally is a mechanism choice
  inside the unit and needs no confirmation.
- A projection unit folds host-side state and declares the client-visible view
  over it, and the registration states that division.
- A partial or cached view may satisfy interface availability, never a visible
  result. Anything the user sees waits for the authoritative value.
- A feature's event handling lives in that feature's own definition, never in
  a central dispatcher. A handler reads only the current event, derives a
  stable identifier for it, folds it into state, and stays deterministically
  replayable by the log's sequence. The append hot path and its renderers
  never scan the whole log; they accumulate into state and publish derived
  facts.
- A definition that consumes a streamed fact implements both the scalar form
  and the packed form of that fact, and it never expands the packed members on
  the incremental path.
- The shared message model is not widened with a business field. A business
  difference travels in the payload — its own extensible source kind or a
  content field — or through a registered renderer.

### Concurrency, timing, and lifecycle

- Correctness comes from explicit call points and explicit state, never from
  where a callback lands in the event loop. A microtask, a timeout, or a
  queued microtask may defer a notification; it never decides whether an effect
  happens.
- Two boundaries: a test's timeout budget that waits for a failure, and a
  deliberate delay that only paces user-visible work, are not timing-dependent
  correctness.
- Async state is not synchronous state. A shared running flag is never the
  result of one unit of work. A caller that owns an interval states that
  interval and its output scope explicitly. Handle the "nothing to wait for"
  branch so a wait cannot hang forever.
- Represent one asynchronous operation with one lifecycle controller or
  transaction. Separate readiness, cancellation, disposal, reservation, or
  sentinel state requires an independent owner or settlement point; otherwise
  fold it into the controller, preserving rollback, callback containment, and
  quiescence.
- Report orthogonal outcomes independently. Surface each fact on its own and
  never nest one flag's report inside another's branch, or a caller reads a
  cut-short run as a clean success. Judge the report by the fields it carries,
  not by the mechanism that produced it.
- The dispatcher catches and reports each listener's failure without rejecting
  the dispatch promise and without starving later listeners. Catching does not
  restore state the listener mutated; the listener's owner handles that.
- Dispose must reach quiescence, not just request it. After killing or
  aborting a child, await its exit; resources and listeners needed for
  completion or final persistence survive until consumers settle. Work with no
  owner to await — a detached, unobserved task — is outside the teardown's
  obligation; anything the component started and can still observe is awaited
  before teardown reports completion.
- Unreversed side effects are refused. Every effect registers its reversal,
  and a registry contribution proves disposal by disposing its owner and
  observing the removal.
- Shared state hangs on a named service or instance with a lifecycle. A
  module-level cache, a singleton, a global weak map, a global symbol
  registry, or a property on a global object bypasses that owner and outlives
  every teardown.
- No global single-selection state exists. A concept that exists once per
  service is carried by a service or instance field, and a global form of it
  is migrated to that owner.
- Passing a shared value across domains goes through the existing interface,
  never a global namespace. If no owner can be named, the owner is decided
  before the implementation; a genuine global lookup key goes to the domain
  owner together with its reclamation policy.
- Context comes from the scope that owns it, and the test is multi-instance
  behavior: two instances under one scope must not cross-talk. Recover the
  owner explicitly at each orchestration entry, derive what that entry needs,
  and let operation-local helpers capture the derived value. This is not a ban
  on capture; it is a ban on ambient reads.
- Keep the session type explicit at lifecycle, logging, service, authority,
  worker, persistence, and wire interfaces. Never widen a leaf helper's
  parameter type merely to hide a parameter.
- Publication has distinct channels. A direct echo of a user gesture publishes
  immediately; a structural update publishes batched; a high-frequency visible
  chunk publishes as a cumulative frame. Batching defers a notification; it
  never decides whether one happens.
- A scoped registration uses an isolated realm. It never mutates the shared
  context and never leaks into a sibling scope.

### State and data integrity

- One value states one fact. Unknown, empty, and absent are three distinct
  states with three distinct representations, and they are never collapsed at a
  boundary. A consumer that must act differently on each discriminates
  explicitly, and the discriminated form is documented where the type is
  defined.
- An optional input states what the callee does when the field is present and
  when it is absent. Both branches state their return value and their side
  effects, and the contract is where the caller reads them. A call site that
  cannot state both behaviors is a review blocker, not a style preference.
- One source of truth for a piece of data, one home for a fact, one owner for
  shared state, and one read path per piece of data.
- Publish state only at its commit point: emit notifications and update
  derived state only after the operation succeeds. Derive caches, prompts,
  echoes, replay, and query views from one authoritative source.
- Retention follows one lifecycle rule per entity type, defined once, where
  the entity type is owned.
- A grouping or placement engine consumes the placement decision as data; the
  caller computes it. Extending the engine means a new placement value, never a
  branch inside the engine.

### Error handling and reporting

- Use typed, structured failures and localized, actionable reports. Define
  handling ownership, containment, and failure guarantees. A report names the
  cause, the consequence, and the next action.
- Normalize every representation of one outcome before it leaves the public
  API, so a consumer can name the source of a failure from the public API
  alone. Internal helpers stay free to throw: the requirement binds the
  boundary the consumer crosses, not every function behind it. Document the
  normalized contract where the type is defined, and exercise every source
  form through the real consumer.
- A missing or stale source produces an explicit outcome instead of silently
  switching onto the slow path. The caller decides what a miss means and states
  it in the returned value.
- Untrusted input the contract cannot interpret is rejected and reported.
  Display data with a documented generic rendering falls back to that
  rendering, and the fallback is itself specified.
- Stop dependent work and visibly report untrusted state or unknown outcomes.
- Misconfiguration fails loud at load when self-contained, otherwise at the
  earliest resolvable point, naming the offending item and the fix. Never
  silently skip a missing referent.
- A genuinely missing configuration is a startup failure with a non-zero exit
  status, not a warning followed by degraded behavior.
- An empty catch names the error and why; keep its try to one statement.
- Trust the static type system at typed same-process boundaries. Do not add
  runtime validation, fallback behavior, or hostile-input tests solely for
  values the static interface requires. Validate at parser, config, queue,
  model/tool data, durable/file, worker, process, and wire boundaries.
- No assertions to unknown. The count of assertions to unknown never grows; a
  replacement uses a typed value or a validation.
- Apply byte, token, item, and time limits to the complete emitted or retained
  value, including wrappers and metadata. Test the exact limit, an oversized
  single chunk, and multibyte sizes.

### Security and trust

- Never hand untrusted output the ambient environment or predictable paths.
- Spawned commands receive a scrubbed environment: variables whose names look
  like keys, secrets, tokens, or passwords are dropped, so harness credentials
  cannot leak into output, environment dumps, or spill files.
- Temporary and spill artifacts use a private owner-only directory, random
  names, and exclusive owner-only creation, so a predictable path cannot be
  replaced or read by another process.
- A path that may be a symlink or a junction is removed by unlinking the link
  itself. The removal primitive deletes only the link and refuses a real
  directory, so it never follows the link into its target. Reserve recursive
  removal for directories known to be real, and for paths the code created
  itself in a directory it owns.
- Credentials are never committed to the repository in any form, including
  example files, fixtures, logs, and recorded output. They are read from the
  environment or an untracked local file.
- A configuration file's code-valued tag is allowed only in the fields that
  document it; every other field stays literal. Conditional composition uses
  an overlay document, not an expression inside the base file.
- A plugin named in a configuration file is also declared in the resolver's
  dependency manifest. A check enforces that the two agree, and every shipped
  configuration is covered by that check.
- A context or working-directory feature supplies paths without changing
  project identity, write roots, or the working directories of existing
  processes. Supplying context and granting access are separate decisions.
- Providers of one capability share a single execution world, so relocating
  that world moves every consumer of the capability without forking the
  provider per consumer.
- Only the project's supported launcher and a named composition start an
  application. A package ships no competing executable, no direct in-process
  mount, and no build-only entry that bypasses the launcher.

### Package surface and build

*These rules bind every package, on every surface.*

- A plugin entry point takes exactly one form, and the two are never mixed: a
  named-export form carrying the loader contract, or a default-exported
  service class. Adding a default export beside the named ones makes the
  loader discard the namespace.
- Beyond the loader contract, a host entry may export the service-definition
  classes and error types other packages extend or throw, and never a
  test-convenience value.
- A browser entry exports what the loader needs and nothing else. A type-only
  export of a store factory or a shared type is allowed; every other new value
  export needs the domain owner's sign-off, not a matching consumer. No other
  feature package imports a browser entry's value.
- A test that needs an internal reaches it through a same-package relative
  path or a documented source subpath, never through a permanent export. A
  fixture never reaches another package by relative path; that bypasses the
  export map and the package-boundary checks.
- A package's export map uses only the standard key set, each key having
  exactly one meaning across the workspace. A package never opens a key for
  its own local purpose. A key used by only a few packages, with no
  documenting owner, is not a precedent. Formalizing a key means changing the
  owning document and the mechanical check in the same change, with the domain
  owner's confirmation.
- A new package has exactly one of two shapes — host-only, or host plus
  client — and both are dynamic plugin packages. No third plugin shape exists.
  Static client libraries outside the dynamic-plugin form are closed to new
  members: they keep working, and no new package joins them. Only a package
  that needs distinct host and client compiler programs splits its compiler
  faces; having both a host entry and a browser entry is not a reason to split.
- Build output, included-file lists, externalized-package lists, and package
  rosters are derived by scanning the workspace, never hand-written, so build
  output and the manifest cannot disagree.
- A package's build configuration extends the project's shared build
  configuration, sets its source and type-output roots, references its
  workspace dependencies, and registers in exactly one aggregate. The types
  module holds type declarations only. Tests live in the package's test
  directory, not inside the source tree.
- Each package belongs to exactly one compiler face. A designated shared leaf
  is the only exception, and both faces compile it. A facade configuration
  never gains an include or file list. A program spanning the repository seeds
  one face explicitly and never the solution root.
- A module starts its asynchronous work inside a function it calls, such as an
  explicit entry that handles its own rejection. A module never awaits at
  module scope: a module-scope wait makes every importer's evaluation wait,
  turns a rejection into a module-evaluation failure, and cannot be lowered to
  a synchronous wrapper form. Executables are exempt only by declaration.
- A new module-resolution hook is refused outright — a definite prohibition,
  not a preference. A genuine need requires both of these together:
  re-auditing the resolution strategy as a whole rather than patching the
  specifier that motivated the change, and the domain owner's recorded
  confirmation. An author's own judgment that a case is special is not a
  confirmation. Wrapping the existing hook to rewrite the same specifiers
  counts as a new hook.
- Internal dependency syntax follows the package family, and the same
  dependency name is written the same way in every workspace. A mismatch is a
  hard mechanical failure, not a warning a reviewer may waive.
- A package's runtime peers appear in matching peer and development sections.
  Browser and type-only relationships are development-only. A host value
  import is classified before it becomes an install dependency. Each build face
  declares its externals independently. The published file list covers every
  relative runtime import and emitted asset.
- Package names carry the organization scope. A vendored copy is rescoped and
  marked private so it is never published. The plugin framework is a peer or
  development dependency, never a bundled runtime dependency. Internal
  dependencies use the workspace protocol.
- An undeclared dependency fails to resolve rather than resolving through
  hoisting. Dependency lifecycle scripts run only when explicitly allowlisted.
- Every module uses the project's standard module system and the package
  declares it. Cross-package imports use the package name; local relative
  imports keep the source extension. No module reachable from the source
  launcher exports a legacy module format only, because native type-stripping
  execution is unavailable across the supported runtime range.
- A package's build adjustments go through the build plugin that owns that
  package's build face; a package never grows its own build branch.
- Build-environment values are a fixed, named set, and the set is recorded
  where the build reads it. Every value referenced at build time is public
  artifact content, unset names resolve to undefined, and no dynamic lookup or
  enumeration exists. A choice that must change after the build is runtime
  configuration.
- A dynamic package's chunks are self-contained: every static relative
  dependency stays inside its owning output chunk, and no chunk keeps a
  synchronous cross-chunk require, because the browser runtime does not
  support that graph.
- Configuration and profile subprocesses run the built artifact under the plain
  runtime, and source regressions use their declared launcher.
- A successful build records the values that identify its output, including a
  digest of the artifacts, and a consumer rejects a missing record or output
  that a later partial build changed.
- A failing build step reports every independent failure and skips only the
  steps that depend on it; unrelated errors are never masked by the first
  failure.

### Performance and measurement

*These rules bind every runtime path, on every surface.*

- Changes that affect a runtime path carry measurement of the changed path,
  not of the whole system. The metrics are runtime, peak memory, and
  steady-state memory, measured in one environment on one input.
- A measurement runs the code in the runtime that ships, never in a development
  source loader; the loader's overhead is part of the number, not part of the
  change under test. A retained-memory claim is taken under a fixed,
  swap-free memory budget, and a package-local diagnostic never joins the
  shared gate.
- The baseline is the same path without the change under measurement — by
  preference the native implementation that carries no measurement hook, never
  the previous implementation measured through the same hook. Attaching the
  same measurement hook to both sides dilutes the difference until it
  disappears while both sides stay slow.
- "Nothing got slower overall" is not evidence, and an improvement elsewhere
  does not excuse a regression on the changed path.
- A green CI benchmark is an alert, not a pass. A regression reported there is
  located by re-running the owning scenario and profiling locally.
- Record the command, the numbers, the environment, and the input size so the
  baseline can be rebuilt.
- An architectural change that genuinely costs performance states the measured
  cost and the reason, and needs the domain owner's approval. "The layering
  needed it" is not a self-service reason.
- Changes that touch no runtime path — comments, documentation, test-only code
  — need no measurement.
- Production performance code is kept. A measurement hook in a production path
  is removed only by the change that removes the path it measures, and that
  change states why the measurement is no longer needed.
- A new incremental computation states its cost in terms of the collection it
  scans and the work per element. "Incremental" without a stated cost model is
  not a claim.
- Worker and concurrency limits are bounded by the resource quota actually
  available to the process, not by the host's processor count, which may exceed
  the quota on a shared runner.

### Configuration and composition

*These rules bind the whole workspace.*

- No hardcoded tunables in plugins. Deployment-varying choices are validated
  configuration fields changeable from configuration. Protocol constants,
  external specs, and security invariants stay fixed. A default constant or a
  test hook is not configurability.
- At a package boundary, defaults are resolved explicitly in one step owned by
  the implementation: a request resolves to a specification, and the execution
  function observes only that specification, never a default of its own.
- Configuration is layered in a fixed, documented order. A layer targets an
  entry by identity and replaces that entry's whole configuration or inserts a
  new entry; fields are never merged one by one. Nested overrides inherit the
  same precedence.
- A package's experimental status is independent of whether it ships by
  default. A non-experimental package needs a real product use or an explicit
  recorded classification.
- A new package's name states a role that exists in the system today, not a
  role that is planned.

---

## Frontend

### Component model

- A component's props come from the framework-derived shares of its
  registration: owner data, declared child slots, a declared store, and the
  inject face. Never hand-write or locally re-type a derived prop member; a
  change to a share's type reaches every consumer of that share.
- The inject face is how a component's data and actions reach it: plain data,
  action callbacks, and a reserved hooks compartment. It is the delivery
  mechanism for every channel, not a channel of its own. Narrow it to what the
  view renders, expose one field per read through a hook seat, and pass a
  narrow action set rather than a store or a service handle.
- A registered component is a stable function component declared at module
  scope. A component created during render is a new type on every pass and
  causes a remount. Registration-time parameters reach the component through
  props, not through closures over render-time variables.
- A remount is a caller-owned decision, visible as a choice in the caller. A
  registered component's identity is stable unless the caller chooses
  otherwise, and a deliberate remount states the state it discards — expanded
  rows, focus, scroll, in-progress input.
- A view component holds presentation and interaction only. Business decisions
  live in the layer that owns them, and everything the component renders arrives
  through its derived prop shares, so the component stays replaceable.
- Components never receive a framework context object, a whole service, or a
  hook or selector as a prop value. A new need becomes a prop threaded from
  its share's source.
- A component's own local state — a fold, a hover, a draft — is not injection
  content.
- A change to a shared contract states its impact on its consumers, including
  which components subscribe to what, in the change itself.

### State and data flow

- Live data has exactly three channels. Data the owner knows travels as owner
  props; data only the view knows stays local; data shared across entries or
  that must survive a remount lives in a declared store. A component resolves
  a need in a fixed order: a framework-bound hook, then a declared store, then
  an injected action. Any other channel is a new framework extension point and
  needs arbitration by the framework owner.
- A view store carries shared viewing and interaction state only. Domain
  objects live in the data layer, and a setting changes through the project's
  settings interface, never through store state.
- A store handle is created inside the registration scope and shared by
  passing that handle to registrations. A module-level handle is a disguised
  singleton across plugin reloads. Persistence is the store's declared key,
  run by the framework and suffixed per instance, so two instances of the same
  store persist independently. The store's declared actions are its complete
  mutation interface, and nothing hydrates a store by hand.
- A reactive source keeps two identities stable: the source object itself, so
  its hook binding is cached, and the snapshot it returns, whose reference
  changes only when the fact changes.
- Whoever rebuilds a published value republishes it through the same source in
  the same step. A registration that can run after consumers exist notifies
  those consumers as part of registering.
- A view never owns a subscription. Reactive facts arrive through a
  framework-bound hook whose lifetime the framework releases. Derived data is
  a pure function over that data — never a second subscription, a mirrored
  snapshot, or a second store.
- The data layer is free of the view framework: it exposes plain data and bare
  reactive sources, never view hooks. Every view hook is composed at the
  binding site, and business code carries no dependency on the render
  machinery.
- A state change does not remount a control or replace its node. The instance
  and its node identity survive it, so focus, selection, and an in-progress
  draft survive it. Initial state is established by the same flow that
  prepares the data, so the first frame already carries it; a later effect
  filling it in is a defect.
- A list key names the entity the element represents, and the change states
  what makes it unique. When two rows genuinely share one identifier, fix the
  identifier's source.
- A view state that can be unknown, empty, or absent keeps three distinct
  values, and a control that cannot show one of them never receives it.
- A value that arrived from a remote or model source is untrusted input. A
  view never renders it as executable markup, and it never becomes a lookup
  key or a command without validation.
- A binary buffer published to a view is borrowed read-only. It is copied
  before it crosses a worker boundary, and it is never written into the
  durable record or the persisted layout.
- A view's context comes from the scope that owns it, and the test is
  multi-instance behavior: two views under one scope must not cross-talk. A
  value genuinely unique across the application is carried by a service or
  instance and injected explicitly rather than reached through an ambient
  field.
- Business code never creates a hook or selector as a prop value; pass plain
  data and callbacks.

### Rendering and composition

- Props carry data and callbacks, not framework nodes. A node travels through
  a declared slot, never as a prop value or injected member. A location that
  accepts another component's markup is declared as a child slot: the host
  passes data only, and the registered component owns its own markup.
- A declared child slot is both declaration and authorization. The slots a
  component renders are exactly the ones its registration declares; rendering
  an undeclared slot, or declaring one another owner declared, fails at load.
  Slot names mirror the composition path.
- The shared list and seat take no new business-specific props. A new node
  shape is admitted by declaring its kind in the project's extensible kind map
  and registering its renderer; the list keeps dispatching on kind.
- A wrapper that needs the standard rendering calls the owning package's
  published entry point and keeps its own fallback. It never re-renders a
  contributed slot and never imports a private component across a package
  boundary.
- The host owns where a contribution appears and how it is invoked. The
  contribution owns its own state, cleanup, and controls. The host never
  carries a business action on behalf of a contribution.
- The default rendering is a registered contribution like any other. The host
  holds no inline fallback copy of a registered contribution.
- An optional override object is applied whole or not at all; it is never
  merged field by field against the default. Omitting it selects the default
  unchanged.
- A shared rendering pipeline carries no business branch. A presentation need
  registers a renderer on the existing chain or expresses itself in the data.
- A feature plugin never runtime-imports or re-exports another feature's
  values. Cross-package behavior crosses through injected services, and
  cross-package interface crosses through declared slots. Shared runtime code
  belongs to one narrow owner package; when neither path fits, the need is
  escalated to the framework owner rather than satisfied with a new export.
- Each shared layer declares the separation it requires, in a list a reviewer
  can check: which imports, which registrations, and which data reads a
  feature may use from it. A shared-layer change regresses every consumer and
  carries the layer owner's confirmation.

### Effects and lifecycle

- An effect never records state and never binds behavior. Both belong to the
  provider or the injected action. An effect may only report an external event
  and clean up after itself.
- Choosing a different effect hook is not a fix; the two differ in timing, not
  in purpose. A measurement effect must first answer why it happens here, why
  the layout cannot express it declaratively, and why the value must be a
  pixel measurement. It then lives in an injected measurement hook, and its
  result is consumed as a declarative layout parameter.
- Timing does not carry correctness. An effect never supplies a "just after
  render" ordering that the design should have made explicit.
- Render-derived data reaches the host one of three ways: it is rendered where
  the state already lives, it is handed over during render through a
  render-time channel, or a user gesture updates the shared state in its own
  handler. An effect body does not call a setter it received through props to
  push render-derived data to a parent, and its cleanup does not reset that
  setter either. An effect that reacts to a genuine external event — a
  subscription callback, a socket frame, a user gesture — is not this pattern.
- A component's cleanup runs when the component unmounts. It releases what
  the component created and never releases a resource the framework owns.
- A component that subscribes to nothing outside the view framework needs no
  new seat.

### Resources and data access

- A resource is read through the project's standard resource hook, addressed by
  its protocol and address. A domain wrapper may reshape the result but never
  rebuilds the fetch, cache, or subscription below it. A bespoke
  subscribe-and-cache per address is refused, because it splits cache and
  invalidation policy, so one address can be live in one place and stale in
  another.
- A client reads the cached projection before the original log and reaches for
  a cold read only when no projection covers the data. The restriction is the
  client's: the host may open a log the client cannot.
- Every session-derived value a client renders arrives through a projection
  read at the render position. A client never fetches session data itself and
  never triggers a computation on demand.
- A client holds a live resource through an explicit, auditable reference. The
  count of references plus in-flight work, both zero, is the only release
  predicate. Reclamation never enumerates which instances may be freed from
  business state: an implicit holder is invisible to the reclaimer, and every
  new business state would have to be added to that list.

### Performance

- A prop that did not change keeps its identity. Watch the mutability of every
  prop: a value rebuilt each render propagates re-renders even when nothing in
  it changed.
- A component subscribes only to the sub-state it reads.
- A high-frequency delta updates only the affected subtree and publishes on a
  batching cadence. Immediate publication is reserved for structural or
  terminal changes, where the cadence is chosen per publication.
- Changes that affect responsiveness carry measurement of the changed path, in
  the metrics the surface exposes: render count, frame time, and the size of
  the state a view holds.

### Accessibility and user experience

- Loading, empty, and error states are explicit surfaces, not silent
  omissions.
- Error copy placement follows the feedback surface rules: a toast for a
  transient outcome the user can keep working through, an in-place notice for
  a problem attached to the content it affects, and an empty state for the
  absence of content. The scope and the persistence of the failure decide
  which one it is.
- A failure that reaches a view is presented as a failure state naming the
  cause and the next action, not as a blank surface, a generic spinner, or a
  partial result. A view that cannot show the failure does not claim success.
- An unknown or malformed payload renders the generic form, and the generic
  form is specified: it is a rendered shape, not an error message, and it
  never presents malformed data as valid.
- Overlay and menu safety: dismissal, focus movement, and keyboard behavior
  are specified at the overlay's declaration, and a change that alters one of
  them changes the declaration with it.
- Platform window adaptation is handled at the layout boundary: a component
  reads the window's size and insets from the layout context, never from a
  global.
- Responsive layouts state the outcome for each case: a value that truncates
  says so, a value that wraps says so, and the focus order survives a width
  change.
- A change that adds a transition or a hover-only affordance preserves visible
  keyboard focus and the project's reduced-motion preference, and a hover-only
  affordance has an equivalent a keyboard user and a touch user can reach.
- Every product-visible string — including accessibility names, tooltips,
  placeholders, status and unit formatters, and primitive chrome — comes from
  a typed locale dictionary. A primitive without a locale binding requires a
  complete label prop and owns no fallback copy.

### Styling and theming

- The theme owns the global scale and every appearance-mode variant. Feature
  styles consume semantic aliases and contain no mode selectors, and a feature
  never defines a second global theme.
- One styling mechanism: feature styling uses the project's component-scoped
  style-module mechanism and its class-composition helper. A component
  library or a utility-class framework is never introduced, and the visual
  system is not extended through one.
- Presentation lives in the style sheet. An inline style object carries only a
  runtime-computed value, and a component-local custom property is allowed
  when the value belongs to that component's own layout or presentation
  contract. A theme branch or a variant switch never appears inline.
- Only images and vector art are inlined, and into the stylesheet rather than
  a markup module. Large text stays in its own module.
- The project's shared control catalog is the only channel that crosses
  feature packages. A deliberate visual difference is a prop on the shared
  control, never a second copy. A new control starts in the feature that needs
  it and is promoted into the shared catalog only when a second consumer
  appears.
- Reuse before adding: extend an existing token or control before creating a
  new one.
- Properties the project's theme style specification validates as a pair are
  always declared together — border and elevation, corner smoothing and
  full-round radius, surface fill and its backdrop filter. The specification's
  allowlist is authoritative: a style it rejects is not corrected by a local
  override.
- Menus and listboxes use the project's shared menu primitive, including custom
  ones. The menu surface material and its backdrop filter are theme-owned, and
  feature and platform styles never override them.
- A modal backdrop is a translucent mask without blur. The theme owns the
  mask's blur setting, and an overlay never filters the page beneath it.
- A type role carries its size and its line height together. A change that
  introduces a size introduces the matching line height and reuses an existing
  typography role when one fits.
- A component whose contract preserves columns keeps its text unwrapped.
  Scrollbar presentation comes from the project's shared scrollbar styles,
  never from component-specific selectors.
- A link uses the theme's link treatment across its resting and hover/focus
  states, and a leading glyph comes from the project's shared icon catalog,
  using the target's own mark when the target publishes one.
- New product artwork follows the project's artwork guide and requires
  designer approval before it lands. Official and experimental artwork follow
  the same process.

### Internationalization

- Client interface copy is locale-owned. Route product text through typed
  dictionaries and the project's localization primitive; hardcoded copy is
  rejected.
- User, model, and wire data stay verbatim. Internal matching — routing,
  comparison, lookup — uses discriminants or stable identifiers, never a
  localized string.
- Locale-owned strings are typed, and a missing key fails at build.
- Translation runs only on explicit request.

---

## Database

### Schema design

- The schema is explicit and versioned. The physical layout version is stored
  on disk and checked on open.
- Tables use strict typing, and foreign keys are enabled.
- Each unit owns its own schema identity. Independent storage providers are
  not coupled through a shared helper.
- Identifiers and table names are validated before interpolation into DDL or
  prepared statements.
- The schema version is monotonic. A non-current, non-zero version rejects
  rather than being migrated in place, unless an adjacent migration exists.
- A schema does not restate a fact another layer owns: no duplicated column
  and no denormalized copy of another owner's field. The stored form is the
  one the owning capability writes.
- Queries are parameterized. No user data is interpolated into a statement.

### Migrations and versioning

- An adjacent migration owns exactly one step. Never move, overwrite, or
  delete a committed generation.
- Predecessors imply neither fallback nor downgrade support.
- Released data formats are never renamed, replaced, or deleted.
- A read selects the highest canonical generation and translates a supported
  historical header without loading records.
- A write encodes, verifies, and exclusively publishes the successor beside
  the unchanged source.
- Ordinary repair of an unsealed interrupted tail remains the handle
  consumer's responsibility. Only the bounded released restart already sealed
  by a later start is repaired by a migration.
- Consumers know only the current logical format. No version-numbered or
  format-specific surface reaches them.
- Bump the version only when existing discriminators cannot prevent unsafe
  interpretation. A same-version change requires safe reader behavior.
- A change to a persisted type is declared and acknowledged in the format's
  compatibility record before it lands, naming the readers it affects.

### Data access and queries

- A read that cannot use the cached path reports the miss rather than taking
  the slow path silently, and a cold read is the last resort.
- A query returns stored rows and stored projections. It never recomputes
  derived state and never reaches into another owner's data.
- Generic orchestration reads projected values. Business rules do not live in
  the data layer.
- Access control filters each row of an aggregate, not only the result set.
- A missing or stale row reports an explicit outcome; the row's owner states
  what the miss means in the returned value.

### Transactions and consistency

- A transaction defines its boundary explicitly. Partial writes are not
  acceptable.
- A cancelled operation leaves no partial durable state. The commit point
  comes after every step that can change what would be written.
- Consistency is stated, not assumed. A reader knows which version it
  observes.
- Concurrent access uses the appropriate isolation, and shared state has one
  owner.
- A write is atomic with its bookkeeping. The version stamp is written last,
  so a failed materialization leaves the medium unstamped and a retry starts
  clean.

### Durability and storage

- Journaling is required for durability. Modes that silently drop durability
  are excluded.
- A database file and its directory are created with restrictive permissions,
  and a file the code did not create keeps the mode it has.
- The storage provider owns the physical layout. Callers ask for a value by
  identity, never by path, and a caller never assembles a file location.
- Compression and framing are owned by the storage provider, not by its
  callers.

### Retention and cleanup

- A stored record is released by the lifecycle rule of its entity type, never
  by a second rule kept in the storage layer.
- The release predicate is "no references and nothing in flight". Reclamation
  never enumerates which instances may be freed from business state.
- A cleanup pass releases the dependents of a record before the record
  itself.
- A retention policy names which of empty, unknown, and absent each entity
  type can be, and treats them differently.

### Security and access

- A stored value is not a trusted input. It is validated at the read boundary
  the same way any other durable input is.
- A database file is never created world-readable, and its directory is never
  world-writable.

### Performance

- Reads are incremental where possible, and a high-frequency delta path
  updates only the affected rows.
- The storage append path never scans the whole record set.
- Performance-sensitive changes carry measurement of the changed path, in the
  metrics the surface exposes: rows scanned, plan chosen, and bytes written.
- Indexes and query plans are deliberate. Do not rely on accidental ordering.

---

## Cross-cutting

### Type safety and documentation

- Compile under the strictest type-check settings with implicit-any
  disabled. Every remaining `any` states why narrowing is infeasible.
- Opaque cross-boundary identifiers are branded, never bare strings. The
  brand is what stops a value from one boundary being passed as a value from
  another.
- A key states what it identifies, and the scope it is unique within. A
  position is not an identity, and a key that exists only to silence a warning
  states an identity the data does not have.
- Reuse before adding. An existing token, control, helper, or rule is
  extended before a second one is created; a second copy is a defect until it
  is shown to need different behavior.
- Every module and export has concise documentation for its non-obvious
  contract. Function-like exports include parameter and return documentation.
  Members inherited from a base type, protocol slots, and constructors are
  documented once, at the type that declares them.
- A package's reference document states its effect on model requests, on
  token consumption, and on cached context, in one canonical section, in the
  same change as the behavior it describes.
- Comments and docs state complete contracts and context, not reasoning
  transcripts. Use direct, concrete terms and no metaphors. Prefer an exact
  term over a generic one. Do not narrate control flow, tests, or review
  history. Keep comments local: do not restate code or expand unrelated
  comments.
- Documentation describes the current state, not the history of how it got
  there. One physical line per paragraph, one home for a fact, and a word
  budget per document. Raise a budget only when the content genuinely needs
  the space, and record why.
- Docs accompany every code change: update the affected README and contract
  documentation together.
- A build names a missing prerequisite and its cause rather than failing
  inside a loader, and the toolchain version is pinned.
- A pasted declaration is bound to its source by an executed check that
  compares the declaration and its documentation. The binding manifest is
  updated in the same change as the document.
- Code comments are written in one language across the codebase. A comment in
  a different language from the code around it is a defect.
- Prefer symmetry for parallel values. Unexplained asymmetry usually signals
  a missed extraction.
- Do not mint names by joining a verb and a suffix into a single word. Name
  the concept with words that can each stand alone.

### Testing and verification

- A claim is falsifiable and recomputable. Match the check to the changed
  surface: focused behavior tests, recorded-output snapshots, documentation
  checks, built smokes for published paths, and real-service end-to-end tests
  for providers.
- Do not default to the full suite, and do not repeat a check that already
  passed. The compatibility matrix and exhaustive coverage belong to the
  continuous integration environment; rehearse everything locally only on
  explicit request, for diagnosing a CI failure, for an irreducibly
  repository-wide change, or after updating a vendored dependency.
- An end-to-end assertion observes the world outside the system under test: it
  re-runs the operation or re-reads the produced artifact. Asserting on the
  system's own report is not evidence. Files the change must not touch are
  asserted byte-identical.
- A test asserts the specified behavior, not the system's report of itself.
  When the specified behavior changes, the expectation changes with it and the
  change states why. An expectation is never weakened, skipped, or deleted to
  keep a run green.
- Substitute only the expensive or nondeterministic boundary — model
  provider, network, clock. Everything downstream of that boundary runs the
  real implementation; a hand-written stand-in for a shipped component is not
  a test of that component.
- A product-visible component requires a test that boots the real composition
  and the shipped configuration, mocking only external services and
  nondeterministic inputs, and asserting model-visible requests, durable
  state, or user-visible output. A hand-assembled service graph is not a
  composition test. Opt-in capability is not enabled in the shipped default
  composition.
- The shipped artifact is the subject of its own test: the built entry point
  runs under the plain runtime, without a source loader that would mask
  startup races, resolution errors, and swallowed load failures.
- A guard only guards if the regression fails it. Introduce the regression it
  is meant to catch, confirm the check fails, then revert.
- Static gates and tests resolve workspace imports to source, so they pass on
  a clean tree; a gate that consumes built output declares that dependency.
  Built output is consumed only where the built artifact is the subject, and
  that exception is stated in the test.
- The coverage gate is complete per file over shipped source. An uncovered
  line is dead code to delete, not a test to bolt on. Coverage proves that
  lines ran, never that the behavior is right; it is necessary and never
  sufficient.
- An excluded line carries a reason naming why the branch cannot be reached. A
  bare exclusion is refused: the branch is either reachable and needs a test,
  or unreachable and needs the reason.
- Behavior that depends on a live model is covered against a live model. A
  keyless run proves plumbing only. A suite that skips without its credential
  is a portability mechanism, never a cost signal or a reason to thin
  coverage.
- Every non-trivial model-, protocol-, or user-visible change updates a
  keyless recorded-session scenario in the same change. Package tests,
  end-to-end tests, mock-only evidence, and written rationale never substitute
  for the assembled transcript.
- Every published surface that projects the driving loop updates its own
  expected output in the same change as a loop, session-lifecycle, or
  event-map change. Each projection is verified by its own suite; the unit
  suite covers none of them, so a projection that is not updated fails nothing
  until a user runs it.
- The tool that regenerates expected output never rewrites the expectation it
  validates, and recording stays out of the shared continuous-integration
  environment, which runs expectations in read-only replay mode. Every
  regenerated difference is reviewed.
- Expected output that is not driven by a recorded session stays with the
  app, package, or script that owns it.
- A fixture that behaves differently on one platform is fixed in the fixture.
  A normalizer that absorbs the difference is refused, because it turns a real
  divergence into a passing run.
- Prefer error paths, event ordering, concurrency races, and permanent
  regression tests over happy-path coverage. A failure that happens before a
  commit point and a failure that happens after it leave the same durable
  state: neither leaves a partial message, a partial tool call, or a partially
  applied turn behind.
- A view spec drives the component through its props and asserts user-visible
  behavior; it never asserts class names, hook internals, or render counts.
  Each test tier asserts its own layer.
- Tests own every resource they acquire — ports, paths, namespaces, spawned
  children — and release it during teardown even on failure, retry, or
  timeout. Process isolation is the only isolation the runner provides. A spec
  that passes only when run alone has a defect in the spec, not in the runner.
- Shared test fixtures live in a dedicated harness module, never in another
  spec file. Importing a spec re-registers its suite and duplicates its side
  effects.
- Tests stay with the code area they exercise. A new capability names every
  required test tier at plan time.
- Every registration registry has a test that disposes a contributor and
  asserts that the cleanup happened.
- A spec that needs a simulated browser environment declares it in that file;
  the shared test configuration stays on the fast default environment.
- A check that consumes built output declares that dependency and runs after
  the artifact build. A check that only type-checks does not rebuild or verify
  artifacts and must not be read as evidence that artifacts are current.
- Before a change is declared complete, the aggregate type check runs against
  artifacts built from the current sources.
- A check that detects stale generated data reports it without rewriting or
  staging the file, so the stale state stays visible and fixable.
- When a required command fails because a sandbox blocks credentials,
  network, IPC, watching, or nested sandboxing, retry the same command with
  the narrowest host escalation and cite the sandbox evidence. Never bypass a
  test failure or a product sandbox control.
- A check that is already red on untouched code is reported in the handoff,
  never silently fixed and never ignored.
- A platform-specific check runs only to diagnose a known failure on that
  platform; the project's compatibility matrix belongs to the continuous
  integration environment.
- The checkout, the installed dependencies, and the toolchain live in the same
  operating-system environment, and native artifacts are never mixed across
  environments. A test result is evidence only for the environment that
  produced it, so platform-specific behavior requires a run on that platform.
- Every runtime version the project claims to support is exercised by the
  project's compatibility matrix.
- A check and its policy own the rule they enforce. When the check fails, fix
  the artifact it names. Relaxing the check, dropping a scanned section, or
  restating the rule in prose are not normal edits: each needs the domain
  owner's confirmation and a recorded reason. Where a general rule and a
  check's policy disagree, the check's policy owns the mechanical question.

### Incomplete work and test doubles

- Shipped code contains no unimplemented behavior. A function whose body
  returns a fixed value, an empty result, a no-op, or a success that means
  nothing does not ship, and it never masks a missing capability. The
  behavior is either implemented, or the capability is absent and the
  operation fails loud with a structured, actionable error naming what is
  missing.
- A capability seam is never satisfied by a stand-in. A fake, stub, or
  in-memory provider registered as a real implementation is a defect, and a
  shipped default composition that registers one is a separate defect. A seam
  without a real provider stays unsatisfied, loudly, and a capability that is
  not ready at all is absent from the composition.
- A stand-in never reaches a shipped path. A double that is only a double is
  named as one where it is registered, and it lives in test-only code or a
  test-support package. Test doubles are governed by the testing rules.
- A temporary is not a plan. A branch, a flag, a configuration default, or an
  environment variable that exists only to be removed later carries an owner
  and a removal condition. A temporary with neither is permanent from the
  first commit, and is reviewed as one.
- A deferred item never stands in for the behavior at hand. The change is
  correct without it. A marker that is the only thing between a caller and
  correct behavior is a defect, not a deferral.
- Known-issue markers are ordered by urgency, and the marker chosen matches
  the urgency, so a reader can distinguish a release blocker from a deferred
  item. A marker also records what the deferred work is, why it is deferred,
  and who owns it; a marker without an owner and a next action is an untracked
  omission, not a deferral.
- Deferred work is tracked where the owning surface keeps its known
  limitations, not only in source. A durable consumer gap or a non-obvious
  maintainer constraint is recorded with the component that owns it; ordinary
  cleanup stays in its marker or its decision record.

### Change management and review

- Split independent changes, and fix the introducing change before propagating
  it downstream. Rewrite a published branch only with a lease-guarded force,
  abort if the remote moved, and never force-push without a lease. Keep an
  in-progress integration checkpoint before taking a newer base.
- A change is not merged before the checks that cover it pass. After a
  published branch is rewritten, the covering checks run again before the
  merge. A handoff reports only the commands that were actually run.
- Commit-time checks validate the content being committed, not the working
  tree, and a machine-applied fix is retried a bounded number of times.
- When a merge check rejects an otherwise clean merge, the result stays staged
  and uncommitted: repair it and commit, or abort the merge explicitly. The
  state is never left for a later, unrelated commit.
- An externally perceptible breaking change is recorded in the surface's
  upgrade guide in the same change, before it lands. The record names the
  change, who it affects, and the step that migrates them. A breaking change
  without that record does not ship.
- Every change carries one kind label, every applicable area label, and the
  tracker's issue type.
- Write a decision record only for durable rationale; mechanical and local
  edits are exempt. Archived records are frozen: never edit them and never
  treat them as current authority.
- Keep the change face equal to the review face: a change carries only the
  lines its stated behavior change needs.
- A package removal removes its build output and any safe residue, so no
  stale artifact can be resolved or measured.
- A vendored third-party copy is tracked with its upstream revision. Update
  it only through the documented sync procedure, re-apply or retire the
  recorded local patches, and re-run the full test suite and build afterwards.
  A change to vendored source is staged together with the manifest update that
  records its upstream revision.
- Every text file ends with exactly one trailing newline, and the
  staged-content check gates it.
- Turn every mechanically checkable invariant into an executed check, and
  prove each changed acceptance path rejects an invalid case. Exceptions are
  narrow and justified, never a global switch that turns a rule off.
- A rule that lives in a mechanical check's policy is not restated as a
  general rule elsewhere.
- Where an instruction file is aliased, edit the real file. Keep each rule
  self-contained and link out for detail.
