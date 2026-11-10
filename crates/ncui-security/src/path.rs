//! Path containment for untrusted, agent-supplied paths (spec 24, 25).
//!
//! # The vulnerability this module exists to prevent
//!
//! A workspace tool is handed path strings by an agent session. If those strings are
//! joined onto a root with the naive `root.join(requested)`, then
//!
//! ```text
//! root     = C:\work
//! requested = ..\..\Users\victim\.ssh\id_rsa
//! result   = C:\work\..\..\Users\victim\.ssh\id_rsa
//! ```
//!
//! and `C:\work\..\..` is `C:\`, so the file read is a read of the user's SSH private
//! key while every log line still shows the path starting with `C:\work`. Containment has
//! to be decided on the **normalised** path, and it has to be decided **per component**.
//!
//! # The trap in the obvious fix
//!
//! The first thing most implementations reach for is `starts_with`:
//!
//! ```text
//! root = C:\work
//! path = C:\workevil
//! ```
//!
//! `"C:\workevil".starts_with("C:\work")` is `true`. That is a *string* prefix, not a
//! *path* prefix: `C:\workevil` is a sibling of `C:\work`, not a child of it, and a
//! sibling is exactly where a sandbox escape goes. [`SafePath::resolve`] therefore
//! compares whole components; [`SafePath::is_contained`] is that comparison, exposed so
//! the property can be tested directly rather than inferred.
//!
//! Case is handled too: on Windows, `c:\work` and `C:\WORK` name the same directory, so
//! component comparison is ASCII-case-insensitive there. On Unix it is not, because Unix
//! paths are case-sensitive and `Case` is a different directory from `case`. A
//! case-insensitive check on Unix would refuse legitimate paths, so the two platforms
//! genuinely need different behaviour rather than one conservative approximation.
//!
//! # Design decisions, stated rather than assumed
//!
//! * **Lexical, not `canonicalize`.** [`SafePath::resolve`] never touches the filesystem.
//!   `std::fs::canonicalize` would *follow symlinks*, which turns "is this path inside the
//!   root" into "where does this path point right now", and that answer can change
//!   between the check and the open. More concretely, `canonicalize` on a non-existent
//!   path is an error, and rejecting-not-found at a security boundary turns a legitimate
//!   create-new-file request into a denial. The residual is explicit and unavoidable
//!   without `openat`-style syscalls, which need `unsafe`: **a symlink planted inside a
//!   root can still point outside it.** Callers that open attacker-influenced paths must
//!   either forbid symlink creation inside a root or re-check with
//!   [`SafePath::is_contained`] against a canonicalized candidate. This module documents
//!   the limitation rather than implying it away.
//! * **Both separator styles, on every platform.** Input may arrive from a Windows agent
//!   running against a Unix build, and `..\\..` is a traversal in both. Splitting only on
//!   the host's native separator would let `..\\..\\etc` through as a single filename
//!   component on Unix — harmless *there*, but the moment that name is used to construct
//!   a path on another host it is a traversal. Normalising both separators uniformly
//!   means the containment semantics of a given string do not change with the platform
//!   that parses it.
//! * **Trailing separators are permitted; interior empty components are not.** `a/b/`
//!   is an ordinary spelling of `a/b` and rejecting it would break well-behaved callers
//!   for no security benefit. `a//b`, however, contains an empty component, and an empty
//!   component is exactly the shape a filter-based implementation gets wrong, so it is
//!   rejected.
//! * **Nothing is normalized away.** `.` and `..` are *resolved* (they are the mechanism
//!   by which traversal is detected) rather than rejected outright, because a path that
//!   climbs above the root must fail as a whole — and because `..` inside the root is
//!   legitimate (`a/b/../c`).

use core::fmt;
use ncui_core::{ErrorKind, NcError, Result};
use std::path::{Component, Path, PathBuf};

/// The operation name recorded on every containment failure.
const OP_RESOLVE: &str = "security::path::resolve";

/// A resolved, containment-checked path.
///
/// `SafePath` is a namespace, not a value: it has no state and no instance identity,
/// because the invariant it provides ("the returned path is inside the root") is a
/// property of the call, and a handle that could be created once and reused for several
/// roots would be a handle that could be used to get the wrong root's guarantee.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SafePath;

impl SafePath {
    /// Joins `requested` onto `root` and returns the result only if it stays inside.
    ///
    /// The algorithm, in order:
    ///
    /// 1. **Reject empty input.** An empty request names the root itself, which is almost
    ///    always a bug in the caller and is never what an agent means by "open this file".
    /// 2. **Reject NUL.** `C:\root\a\0` is a classic truncation attack against a Win32 API
    ///    that is later reached with a NUL-terminated string, and `std::path` will happily
    ///    construct it.
    /// 3. **Reject an absolute or drive-qualified or UNC `requested`.** Detected on the
    ///    *string*, before any joining: `Path::is_absolute` is platform-relative, so on
    ///    Unix it says `false` for `\Windows\System32` — which is not an absolute path on
    ///    Unix, and this module rejects it anyway so that a string's verdict does not
    ///    depend on which host parses it. Joining an absolute path onto a root *discards*
    ///    the root in `std`, so this check is not cosmetic.
    /// 4. **Split on both separators** into components, rejecting empty interior ones.
    /// 5. **Resolve `.` and `..` lexically** against the *root's* components, never by
    ///    string surgery on the joined result.
    /// 6. **Verify by component** that the result is inside the root, as a defence in
    ///    depth: step 5 cannot escape by construction, and an assertion that re-derives
    ///    the property from the final components catches a future change to step 5.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] for every rejection above, and
    /// [`ErrorKind::Internal`] if step 6 fails, which would mean the resolver has a bug —
    /// reported as an internal error rather than silently returning the path.
    pub fn resolve(root: &Path, requested: &str) -> Result<PathBuf> {
        if requested.is_empty() {
            return Err(rejected("requested", "the requested path is empty"));
        }
        if requested.contains('\0') {
            return Err(rejected(
                "requested",
                "the requested path contains a NUL byte, which truncates the path for \
                 C string APIs and is never legitimate",
            ));
        }
        if has_absolute_form(requested) {
            return Err(rejected(
                "requested",
                "the requested path is absolute, drive-qualified or UNC; only paths \
                 relative to the root may be resolved. Note that joining an absolute path \
                 onto a root discards the root entirely",
            ));
        }

        let root_components = Self::root_components(root)?;
        let resolved = Self::resolve_components(&root_components, requested)?;

        // Defence in depth. The resolver above cannot escape, so this is an invariant
        // check on the final components rather than the primary control. It is checked
        // on `resolved` (not on the joined string) so that it is exactly the property the
        // caller depends on.
        let joined = join_components(root, &resolved);
        if !Self::is_contained(root, &joined) {
            return Err(NcError::new(ErrorKind::Internal {
                operation: OP_RESOLVE.to_owned(),
                reason: format!(
                    "resolver produced {}, which is not contained by {}; \
                     this indicates a defect in security::path::SafePath",
                    joined.display(),
                    root.display()
                ),
            }));
        }
        Ok(joined)
    }

    /// Whether `candidate` is `root` itself or a descendant of it.
    ///
    /// The comparison is **per component** and, on Windows, case-insensitive — see the
    /// module documentation for why neither string prefixing nor case folding everywhere
    /// is correct. This is the predicate the whole module exists to get right, so it is
    /// public and tested directly rather than being buried inside `resolve`.
    ///
    /// A *canonical, lexical* comparison: it says nothing about symlinks. Both arguments
    /// should already be normalised (lexically free of `.` and `..`); when they are not,
    /// the comparison is conservative — an unresolved `..` fails to match and the answer is
    /// `false`, which is the safe direction.
    ///
    /// That conservatism is explicit rather than incidental. `Path::components` does not
    /// collapse `..`, so a candidate carrying one produces a `ParentDir` component that can
    /// never equal a root component. An earlier version relied on that mismatch, but the
    /// root-exhausted arm below returns before reaching it: once the root's components run
    /// out, everything the candidate has left is accepted unexamined — so
    /// `is_contained("/srv/work", "/srv/work/../../etc/passwd")` answered `true`. The doc
    /// claimed the opposite of the behaviour, and this function is named in the module
    /// documentation as the mitigation for the symlink gap, so a caller following the
    /// documented advice would have been handed a permissive answer. `ParentDir` is now
    /// rejected wherever it appears.
    #[must_use]
    pub fn is_contained(root: &Path, candidate: &Path) -> bool {
        let mut root_parts = root.components();
        let mut candidate_parts = candidate.components();
        loop {
            match (root_parts.next(), candidate_parts.next()) {
                // The candidate ran out while the root still has components: the
                // candidate is a strict prefix of the root, i.e. an ancestor of it.
                // That is *outside*, not inside.
                (Some(_), None) => return false,
                // The root ran out: everything the candidate has left is deeper, hence
                // inside — *provided none of it climbs back out*. `root == candidate` lands
                // here on the first iteration, with nothing left to inspect.
                //
                // The remaining components still have to be checked rather than trusted.
                // Once the root is exhausted this arm decides the answer alone, so a `..`
                // sitting past the root — `/srv/work/../etc/passwd` — would otherwise be
                // accepted along with the genuinely deeper tail it is mixed in with. A `..`
                // that climbs above the root makes the whole candidate outside it, however
                // many real components follow it.
                (None, Some(part)) => {
                    // The root is exhausted, so this arm decides the answer for everything
                    // below it — and `part` has already been consumed off the iterator by
                    // the match, so it has to be checked here as well as in the tail. This is
                    // the single `..` case: `/srv/work/../etc/passwd` reaches this arm with
                    // `part` *being* the `..` that leaves the root, so inspecting only the
                    // tail after it would accept a path that climbed out.
                    return part != Component::ParentDir
                        && candidate_parts.all(|tail| tail != Component::ParentDir);
                }
                // The root and the candidate ran out together: identical paths.
                (None, None) => return true,
                (Some(root_part), Some(candidate_part)) => {
                    // A candidate that has not been normalised is refused outright. `..`
                    // cannot be compared for equality against any root component, so the
                    // comparison below would reject it — but only if the walk reaches it,
                    // and the root-exhausted arm returns first. Checking here makes the
                    // guarantee independent of where in the walk the `..` appears, which is
                    // what the doc has always claimed.
                    if candidate_part == Component::ParentDir {
                        return false;
                    }
                    if !Self::components_equal(root_part, candidate_part) {
                        return false;
                    }
                }
            }
        }
    }

    /// Compares one root component against one candidate component.
    ///
    /// The comparison runs over the components' *encoded bytes*, not over lossy UTF-8 text.
    /// `to_string_lossy` replaces every invalid sequence with U+FFFD, so two different
    /// names made of different invalid bytes both became the same replacement character and
    /// compared equal — a containment check that declared distinct components identical,
    /// silently and in the permissive direction. `as_encoded_bytes` is the lossless view of
    /// the same bytes, and it needs no `unsafe`, which this workspace forbids.
    fn components_equal(root_part: Component<'_>, candidate_part: Component<'_>) -> bool {
        let root_bytes = root_part.as_os_str().as_encoded_bytes();
        let candidate_bytes = candidate_part.as_os_str().as_encoded_bytes();
        if cfg!(windows) {
            // Windows path comparison is case-insensitive; `C:\Work` and `c:\work` are
            // the same directory and a check that disagreed would either refuse a
            // legitimate path or, worse, be loosened until it did not.
            //
            // ASCII case folding is used rather than a Unicode fold because Windows itself
            // compares the underlying code units case-insensitively; folding beyond ASCII
            // would call two distinct Windows filenames equal.
            if root_bytes.len() != candidate_bytes.len() {
                return false;
            }
            root_bytes
                .iter()
                .zip(candidate_bytes.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        } else {
            // Unix paths are case-sensitive: `Case` and `case` are different files, so
            // folding here would refuse a path that exists.
            root_bytes == candidate_bytes
        }
    }

    /// The root's own components, checked for the shapes a root must not have.
    ///
    /// A root is trusted input in a way that `requested` is not — it comes from
    /// configuration — but it is still validated, because a root of `C:\work\..` would
    /// make every containment answer a lie, and a root containing `..` cannot be
    /// compared component-wise at all.
    fn root_components(root: &Path) -> Result<Vec<String>> {
        if root.as_os_str().is_empty() {
            return Err(rejected("root", "the root path is empty"));
        }
        if root.as_os_str().to_string_lossy().contains('\0') {
            return Err(rejected("root", "the root path contains a NUL byte"));
        }
        let mut parts = Vec::new();
        for component in root.components() {
            match component {
                // A root may legitimately be absolute (`/srv/work`, `C:\work`) or start
                // with a drive prefix or a UNC share; those are the root's identity and
                // are preserved verbatim.
                Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                    parts.push(component.as_os_str().to_string_lossy().into_owned());
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(rejected(
                        "root",
                        "the root path contains '..'; a root that is not already resolved \
                         cannot support containment checks",
                    ));
                }
            }
        }
        if parts.is_empty() {
            return Err(rejected(
                "root",
                "the root path has no components, so nothing can be contained by it",
            ));
        }
        Ok(parts)
    }

    /// Resolves `requested` against `root_components`, returning the surviving components.
    ///
    /// This is the step that makes `..` safe. `..` pops the *resolution stack*, so it can
    /// only escape if the stack is already at the root — and popping an empty stack is the
    /// rejection. Crucially, the stack starts as the root's own components, so
    /// `a/../../b` pops `a` and then tries to pop the root and is rejected, rather than
    /// silently producing a sibling of the root.
    fn resolve_components(root_components: &[String], requested: &str) -> Result<Vec<String>> {
        // The stack starts as a copy of the root's components so that `..` can never walk
        // past it: the traversal is measured against the root, not against the parent of
        // whatever happens to be on the stack.
        let mut stack: Vec<String> = root_components.to_vec();
        let depth_of_root = stack.len();

        for raw in split_components(requested) {
            match raw {
                "" => {
                    // An empty interior component. Rejected rather than filtered: a
                    // filter-based implementation silently turns `a//b` into `a/b`, and
                    // `..`-counting implementations that skip empties mis-count depth.
                    // Neither failure is obvious, so the shape is refused outright.
                    return Err(rejected(
                        "requested",
                        "the requested path contains an empty component (a doubled \
                         separator); only a single trailing separator is permitted",
                    ));
                }
                "." => {
                    // Resolved away. `.` is a no-op that must not push a component,
                    // because a pushed empty component is what a `..`-based escape
                    // counts on.
                }
                ".." => {
                    if stack.len() <= depth_of_root {
                        return Err(rejected(
                            "requested",
                            "the requested path traverses above the root with '..'",
                        ));
                    }
                    let popped = stack.pop();
                    debug_assert!(popped.is_some(), "stack length was checked above");
                }
                component => {
                    // A component containing a colon would be reinterpreted when
                    // joined: on Windows `PathBuf::push("c:d")` parses as a drive prefix
                    // and *discards the accumulated path entirely*, so `C:\work` becomes
                    // `c:d`. The colon is also the NTFS alternate-data-stream operator
                    // (`file.txt:hidden`), which is a second way to name a stream the
                    // name never mentioned. Colons are rejected everywhere, on every
                    // platform, rather than only where the host happens to parse them —
                    // a component that means one thing on Windows and another on Unix is
                    // exactly the shape a replay across hosts exploits.
                    if component.contains(':') {
                        return Err(rejected(
                            "requested",
                            "a path component contains ':', which is a drive separator, a \
                             UNC marker or an alternate-data-stream operator depending on \
                             the host; no legitimate workspace filename needs one",
                        ));
                    }
                    stack.push(component.to_owned());
                }
            }
        }

        let relative = stack.split_off(depth_of_root);
        if relative.is_empty() {
            return Err(rejected(
                "requested",
                "the requested path resolves to the root itself, which names no file \
                 within it",
            ));
        }
        Ok(relative)
    }
}

/// Splits `requested` on both `/` and `\`, rejecting empty interior components.
///
/// A single trailing separator is dropped (so `a/b/` and `a/b` are the same request), and
/// every other empty component is rejected. Interior empties are not a cosmetic issue:
/// an implementation that filters components would turn `a//b` into `a/b` correctly, but
/// one that splits and re-joins without checking produces a path with an empty component
/// in it, which the Win32 path parser treats specially.
fn split_components(requested: &str) -> Vec<&str> {
    let trimmed = requested
        .strip_suffix('/')
        .or_else(|| requested.strip_suffix('\\'))
        .unwrap_or(requested);
    trimmed.split(['/', '\\']).collect()
}

/// Whether `requested` has the *shape* of an absolute path on any supported platform.
///
/// Checked on the string rather than with [`Path::is_absolute`] because that predicate is
/// platform-relative: it answers `false` for `\Windows\System32` on Unix and for
/// `C:\Windows` on Unix, both of which are absolute on the platform whose filesystem they
/// would eventually reach. A containment check whose verdict changes with the host it
/// runs on is not a containment check.
fn has_absolute_form(requested: &str) -> bool {
    if requested.starts_with('/') || requested.starts_with('\\') {
        // A leading separator is a rooted path (`/etc`, `\Windows`) or, with two, a UNC
        // path (`\\server\share`).
        return true;
    }
    // Drive-qualified: `C:\...` (absolute) and `C:...` (drive-relative, which `std` also
    // resolves by discarding the base). Both are rejected: the first escapes trivially,
    // the second escapes in a way that depends on the process's current drive.
    let bytes = requested.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Joins components onto `root`, choosing separators appropriate to the root's own form.
///
/// The result is a lexical join: no canonicalization, no filesystem access.
fn join_components(root: &Path, relative: &[String]) -> PathBuf {
    let mut joined = PathBuf::from(&root_components_as_path(root));
    for component in relative {
        joined.push(component);
    }
    joined
}

/// Reconstructs a [`PathBuf`] from a root's components.
///
/// Done by joining onto an empty buffer with an explicit separator prefix so that a root
/// of `/srv/work` stays absolute and `C:\work` keeps its drive, which `PathBuf::push`
/// alone would not guarantee when handed components piecemeal.
fn root_components_as_path(root: &Path) -> PathBuf {
    let mut path = PathBuf::new();
    for component in root.components() {
        path.push(component.as_os_str());
    }
    path
}

/// Builds a [`ErrorKind::Validation`] error for a rejected path.
///
/// The message never echoes the offending path: a rejected path is attacker-controlled
/// text, and putting it in an error message puts it in a log line (spec 28's concern, in
/// the specific case of a value that may be large, may contain terminal control
/// sequences, and tells an attacker exactly which filter tripped).
fn rejected(field: &str, reason: &str) -> NcError {
    NcError::new(ErrorKind::Validation {
        field: field.to_owned(),
        reason: reason.to_owned(),
    })
    .operation(OP_RESOLVE)
}

impl fmt::Display for SafePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SafePath")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic PRNG (xorshift64*), so a failure is reproducible from the seed.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next_u64() % bound
        }
    }

    /// Roots chosen so the string-prefix trap is exercised on whichever separator the
    /// host uses. `C:\rootevil` is a sibling of `C:\root`, not a child of it.
    const UNIX_ROOT: &str = "/srv/work";
    const WINDOWS_ROOT: &str = "C:\\work";

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            WINDOWS_ROOT
        } else {
            UNIX_ROOT
        })
    }

    fn sibling() -> PathBuf {
        let base = root();
        let mut name = base.file_name().expect("root has a name").to_os_string();
        name.push("evil");
        base.with_file_name(name)
    }

    fn assert_contained(requested: &str) -> PathBuf {
        let resolved = SafePath::resolve(&root(), requested)
            .unwrap_or_else(|error| panic!("expected {requested:?} to resolve: {error}"));
        assert!(
            SafePath::is_contained(&root(), &resolved),
            "{requested:?} resolved to {resolved:?}, which escaped {:?}",
            root(),
        );
        resolved
    }

    fn assert_rejected(requested: &str) {
        let error = SafePath::resolve(&root(), requested)
            .expect_err(&format!("expected {requested:?} to be rejected"));
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. })
                || matches!(error.kind(), ErrorKind::Internal { .. }),
            "{requested:?} was rejected but not with a validation error: {error:?}",
        );
        assert_eq!(error.operation_name(), OP_RESOLVE);
    }

    // ---- the prefix trap ----

    /// The classic mistake: `C:\workevil` is a *sibling* of `C:\work`, and a string
    /// prefix check says it is inside. The component comparison must say otherwise.
    #[test]
    fn a_sibling_whose_name_extends_the_root_is_not_contained() {
        let root_path = root();
        let sibling_path = sibling();

        // Establish that the string-prefix test really does fail to catch this, so this
        // test cannot silently stop testing anything.
        let root_text = root_path.to_string_lossy();
        let sibling_text = sibling_path.to_string_lossy();
        assert!(
            sibling_text.starts_with(root_text.as_ref()),
            "premise broken: {sibling_text:?} does not start with {root_text:?}, so the \
             string-prefix trap is no longer being exercised"
        );
        assert!(
            !SafePath::is_contained(&root_path, &sibling_path),
            "{sibling_path:?} was treated as contained by {root_path:?}"
        );
    }

    #[test]
    fn the_root_itself_is_contained_in_the_root() {
        let root_path = root();
        assert!(SafePath::is_contained(&root_path, &root_path));
    }

    #[test]
    fn a_child_of_the_root_is_contained() {
        let root_path = root();
        let child = root_path.join("src");
        assert!(SafePath::is_contained(&root_path, &child));
        let grandchild = child.join("main.rs");
        assert!(SafePath::is_contained(&root_path, &grandchild));
    }

    #[test]
    fn an_ancestor_of_the_root_is_not_contained() {
        let root_path = root();
        let parent = root_path.parent().expect("root has a parent").to_path_buf();
        assert!(!SafePath::is_contained(&root_path, &parent));
    }

    #[test]
    fn a_prefix_that_ends_mid_component_is_not_contained() {
        // `/srv` is a textual prefix of `/srv/work` and an actual ancestor.
        let root_path = PathBuf::from("/srv/work");
        assert!(!SafePath::is_contained(&root_path, Path::new("/srv")));
        // `/srv/wor` and `/srv/works` both extend the root's *text* and are outside it.
        assert!(!SafePath::is_contained(&root_path, Path::new("/srv/works")));
        assert!(!SafePath::is_contained(&root_path, Path::new("/srv/wor")));
    }

    #[test]
    fn a_candidate_that_climbs_out_with_a_parent_component_is_not_contained() {
        // Unix-shaped, so the assertion holds on every host OS.
        let root_path = PathBuf::from("/srv/work");
        // The multi-`..` case reached the root-exhausted arm with the `..` already past it.
        assert!(!SafePath::is_contained(
            &root_path,
            Path::new("/srv/work/../../etc/passwd")
        ));
        // The single-`..` case is the one a tail-only check misses: the `..` *is* the first
        // component the root-exhausted arm receives, so a check that only inspected the rest
        // of the iterator accepted a path that left the root.
        assert!(!SafePath::is_contained(
            &root_path,
            Path::new("/srv/work/../etc/passwd")
        ));
        // A `..` mixed in with genuinely deeper components is still an escape: how much of
        // the candidate is legitimately below the root does not change where it lands.
        assert!(!SafePath::is_contained(
            &root_path,
            Path::new("/srv/work/src/../../../../root/.ssh/id_rsa")
        ));
    }

    #[test]
    fn a_candidate_with_a_parent_component_is_refused_even_when_it_also_looks_deeper() {
        let root_path = PathBuf::from("/srv/work");
        // The `.`-only form is fine and stays accepted; only `..` is refused.
        assert!(SafePath::is_contained(
            &root_path,
            Path::new("/srv/work/./src/main.rs")
        ));
        // A trailing `..` normalises back to the root, so this is inside — but it is still
        // an unnormalised input, and refusing it is the conservative direction the doc
        // promises. `resolve` is the path that normalises; this predicate only judges.
        assert!(!SafePath::is_contained(
            &root_path,
            Path::new("/srv/work/src/..")
        ));
    }

    #[test]
    fn two_different_components_built_from_invalid_encodings_are_not_equal() {
        // `to_string_lossy` maps every invalid sequence to U+FFFD, so these two distinct
        // names compared equal and containment accepted the wrong one. The comparison runs
        // over encoded bytes, which are lossless.
        //
        // Unpaired UTF-16 surrogates on Windows and invalid UTF-8 on Unix: each platform's
        // native way to name a path component that is not valid text, built without `unsafe`
        // because this workspace forbids it.
        #[cfg(windows)]
        let (root_os, candidate_os) = {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;
            // U+D800 and U+D801 as raw code units: unpaired, distinct, and both
            // lossy-convert to U+FFFD. Written as literals rather than as `\u{...}`
            // escapes because a lone surrogate is not a valid Rust character literal.
            (
                OsString::from_wide(
                    &"root"
                        .encode_utf16()
                        .chain(std::iter::once(0xD800u16))
                        .collect::<Vec<_>>(),
                ),
                OsString::from_wide(
                    &"root"
                        .encode_utf16()
                        .chain(std::iter::once(0xD801u16))
                        .collect::<Vec<_>>(),
                ),
            )
        };
        #[cfg(unix)]
        let (root_os, candidate_os) = {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let mut root_name = Vec::from(&b"root"[..]);
            root_name.extend_from_slice(&[0xED, 0xA0, 0x80]);
            let mut candidate_name = Vec::from(&b"root"[..]);
            candidate_name.extend_from_slice(&[0xED, 0xA0, 0x81]);
            (
                OsStr::from_bytes(&root_name).to_os_string(),
                OsStr::from_bytes(&candidate_name).to_os_string(),
            )
        };

        assert_ne!(root_os, candidate_os, "the two names must genuinely differ");
        assert!(
            !SafePath::is_contained(Path::new(&root_os), Path::new(&candidate_os)),
            "two different component names were declared equal, so containment accepted one \
             as a child of the other"
        );
    }

    // ---- accepted, ordinary inputs ----

    #[test]
    fn a_simple_relative_path_resolves_under_the_root() {
        let resolved = assert_contained("src/main.rs");
        assert_eq!(resolved, root().join("src").join("main.rs"));
        assert!(resolved.ends_with("main.rs"));
    }

    #[test]
    fn a_single_component_resolves_under_the_root() {
        assert_eq!(assert_contained("Cargo.toml"), root().join("Cargo.toml"));
    }

    #[test]
    fn both_separator_styles_are_accepted_and_normalized() {
        let slash = assert_contained("src/main.rs");
        let backslash = assert_contained("src\\main.rs");
        assert_eq!(slash, backslash, "separator style changed the result");

        let mixed = assert_contained("src\\lib\\mod.rs");
        assert_eq!(mixed, root().join("src").join("lib").join("mod.rs"));
    }

    #[test]
    fn a_single_trailing_separator_is_permitted() {
        assert_eq!(assert_contained("src/"), root().join("src"));
        assert_eq!(assert_contained("src\\"), root().join("src"));
    }

    #[test]
    fn interior_dot_components_are_resolved_away() {
        assert_eq!(assert_contained("./src"), root().join("src"));
        assert_eq!(
            assert_contained("src/./main.rs"),
            root().join("src").join("main.rs")
        );
        assert_eq!(assert_contained("./././src"), root().join("src"));
    }

    #[test]
    fn interior_parent_components_are_resolved_when_they_stay_inside() {
        assert_eq!(
            assert_contained("src/lib/../main.rs"),
            root().join("src").join("main.rs")
        );
        assert_eq!(
            assert_contained("a/b/c/../../d"),
            root().join("a").join("d")
        );
    }

    #[test]
    fn names_containing_dots_are_not_mistaken_for_traversal() {
        // `.gitignore` is a single component that merely starts with a dot.
        assert_eq!(assert_contained(".gitignore"), root().join(".gitignore"));
        assert_eq!(assert_contained("..hidden"), root().join("..hidden"));
        assert_eq!(
            assert_contained("a/..b/c"),
            root().join("a").join("..b").join("c")
        );
        assert_eq!(assert_contained("..."), root().join("..."));
        assert_eq!(assert_contained("a.b/c.d"), root().join("a.b").join("c.d"));
    }

    // ---- rejections ----

    #[test]
    fn an_empty_request_is_rejected() {
        assert_rejected("");
    }

    #[test]
    fn nul_bytes_are_rejected() {
        assert_rejected("src/main.rs\0");
        assert_rejected("\0");
        assert_rejected("src/\0/../../etc/passwd");
        assert_rejected("..\0..");
    }

    #[test]
    fn absolute_paths_are_rejected() {
        assert_rejected("/etc/passwd");
        assert_rejected("/");
        assert_rejected("\\Windows\\System32"); // ncui-allow-hygiene: rejected-path test vector
        assert_rejected("\\");
    }

    #[test]
    fn windows_drive_qualified_paths_are_rejected() {
        assert_rejected("C:\\Windows\\System32\\config");
        assert_rejected("C:/Windows");
        assert_rejected("C:");
        assert_rejected("c:\\windows");
        assert_rejected("Z:\\other");
    }

    #[test]
    fn unc_paths_are_rejected() {
        assert_rejected("\\\\server\\share\\secret"); // ncui-allow-hygiene: rejected-path test vector
        assert_rejected("\\\\?\\C:\\Windows");
        assert_rejected("\\\\server");
    }

    #[test]
    fn parent_traversal_above_the_root_is_rejected() {
        assert_rejected("..");
        assert_rejected("../");
        assert_rejected("../outside");
        assert_rejected("../../etc/passwd");
        assert_rejected("..\\..\\Windows");
        assert_rejected("a/../../outside");
        assert_rejected("a/b/../../../outside");
        assert_rejected("src/..");
        assert_rejected("./../outside");
        assert_rejected("a/./../../outside");
    }

    #[test]
    fn interior_empty_components_are_rejected() {
        assert_rejected("a//b");
        assert_rejected("a\\\\b");
        assert_rejected("a//../b");
        assert_rejected("//");
        assert_rejected("///a");
    }

    #[test]
    fn a_request_that_resolves_to_the_root_itself_is_rejected() {
        // Naming the container rather than something inside it is a caller bug and would
        // otherwise hand back a directory that the caller then tries to read.
        assert_rejected(".");
        assert_rejected("./");
        assert_rejected("a/..");
        assert_rejected("a/b/../..");
    }

    /// A colon anywhere in a path component is rejected on every platform: on Windows it
    /// is a drive separator (`a:b` is drive-relative), a UNC marker, or an
    /// alternate-data-stream operator (`file.txt:hidden`); on Unix it is an ordinary
    /// character. Rejecting uniformly means a component's meaning does not change with the
    /// host that parses it, which is the property the whole module rests on. A path that
    /// works on Unix and escapes on Windows is a latent vulnerability, not a portability
    /// nicety.
    #[test]
    fn colons_are_rejected_in_every_component_position() {
        assert_rejected("a:b/c");
        assert_rejected("a:");
        assert_rejected("x/a:b/c");
        assert_rejected("a/b:c");
        assert_rejected("file.txt:hidden");
        assert_rejected("..:/x");
    }

    #[test]
    fn deeply_nested_and_unicode_requests_that_stay_inside_are_accepted() {
        let deep = "a/".repeat(64) + "leaf.txt";
        assert!(SafePath::is_contained(&root(), &assert_contained(&deep)));
        assert!(SafePath::is_contained(
            &root(),
            &assert_contained("ünïcode/файл.rs")
        ));
        assert!(SafePath::is_contained(
            &root(),
            &assert_contained("a b/c d/e f.txt")
        ));
    }

    // ---- root validation ----

    #[test]
    fn an_invalid_root_is_rejected() {
        assert!(SafePath::resolve(Path::new(""), "a").is_err());
        assert!(SafePath::resolve(Path::new("."), "a").is_err());
        assert!(SafePath::resolve(Path::new("../work"), "a").is_err());
        assert!(SafePath::resolve(Path::new("/a/../b"), "a").is_err());
        assert!(SafePath::resolve(Path::new("a\0b"), "a").is_err());
    }

    // ---- the property test ----

    /// **The most important test in this crate.**
    ///
    /// Generates adversarial path strings from a deterministic PRNG over an alphabet of
    /// traversal, separator, drive, NUL and Unicode characters, and asserts the one
    /// invariant that must never break: `SafePath::resolve` either returns a path
    /// contained by the root, or returns an error. It must never return a path outside
    /// the root.
    ///
    /// The generator deliberately includes multi-byte characters and an invalid UTF-8-like
    /// sequence so that a fix which only holds for ASCII does not pass.
    #[test]
    fn randomized_paths_never_escape_the_root() {
        // Characters chosen for maximum damage: separators in both styles, the traversal
        // pair, a drive colon, NUL, and non-ASCII bytes.
        const ALPHABET: &[&str] = &[
            "a", "b", "root", "evil", ".", "..", "/", "\\", ":", "\0", " ", "~", "%2e%2e", "é",
            "ф", "C:", "\\\\", "//", "...", "....", "\u{202e}", "\u{0}",
        ];
        let mut rng = Rng(0x5DEE_CE66_D1CE_4F5D);
        let mut resolved_count = 0u64;
        let mut rejected_count = 0u64;

        for _ in 0..50_000 {
            let length = 1 + rng.below(12);
            let mut requested = String::new();
            for _ in 0..length {
                let piece = ALPHABET[rng.below(ALPHABET.len() as u64) as usize];
                requested.push_str(piece);
            }

            let root_path = root();
            match SafePath::resolve(&root_path, &requested) {
                Ok(resolved) => {
                    resolved_count += 1;
                    // The invariant: the returned path is inside the root.
                    assert!(
                        SafePath::is_contained(&root_path, &resolved),
                        "ESCAPE: root={root_path:?} requested={requested:?} \
                         resolved={resolved:?} (iteration escaped the root)"
                    );
                    // The returned path must also carry no unresolved traversal or NUL.
                    for component in resolved.components() {
                        let text = component.as_os_str().to_string_lossy();
                        assert_ne!(text.as_ref(), "..", "unresolved '..' in {resolved:?}");
                        assert_ne!(text.as_ref(), ".", "unresolved '.' in {resolved:?}");
                        assert!(!text.contains('\0'), "NUL in {resolved:?}");
                    }
                }
                Err(error) => {
                    rejected_count += 1;
                    assert!(
                        matches!(error.kind(), ErrorKind::Validation { .. })
                            || matches!(error.kind(), ErrorKind::Internal { .. }),
                        "unexpected error for {requested:?}: {error:?}"
                    );
                }
            }
        }

        // The generator must be producing both outcomes, or the test is not exercising
        // the interesting branch. A generator that rejected everything would pass the
        // invariant trivially.
        assert!(
            resolved_count > 0 && rejected_count > 0,
            "degenerate generator: {resolved_count} resolved, {rejected_count} rejected"
        );
        assert_eq!(resolved_count + rejected_count, 50_000);
    }

    /// The same invariant, with a generator biased toward `..` and separators so that
    /// escapes are attempted far more often than in the uniform test.
    #[test]
    fn randomized_paths_biased_toward_traversal_never_escape() {
        const HOSTILE: &[&str] = &["..", "../", "..\\", "/", "\\", "../..", "a", "..%2f"];
        let mut rng = Rng(0x0BAD_C0DE_DEAD_BEEF);
        let root_path = root();
        let mut escapes_attempted = 0u64;

        for _ in 0..50_000 {
            let length = 1 + rng.below(8);
            let mut requested = String::new();
            for _ in 0..length {
                requested.push_str(HOSTILE[rng.below(HOSTILE.len() as u64) as usize]);
            }
            if requested.contains("..") {
                escapes_attempted += 1;
            }
            if let Ok(resolved) = SafePath::resolve(&root_path, &requested) {
                assert!(
                    SafePath::is_contained(&root_path, &resolved),
                    "ESCAPE: requested={requested:?} resolved={resolved:?}"
                );
            }
        }
        assert!(
            escapes_attempted > 1_000,
            "generator produced too few traversal attempts: {escapes_attempted}"
        );
    }

    /// A root that is a *parent* of a nested working directory must contain it, and a
    /// sibling of it must not. This is the realistic deployment shape.
    #[test]
    fn nested_roots_behave_correctly() {
        let base = if cfg!(windows) {
            PathBuf::from("C:\\work")
        } else {
            PathBuf::from("/srv/work")
        };
        let project = base.join("project");
        assert!(SafePath::is_contained(&base, &project));
        assert!(SafePath::is_contained(&project, &project.join("src")));
        assert!(!SafePath::is_contained(&project, &base));
        assert!(!SafePath::is_contained(&project, &base.join("other")));

        let resolved = SafePath::resolve(&project, "src/main.rs").expect("resolves");
        assert_eq!(resolved, project.join("src").join("main.rs"));
        assert!(SafePath::resolve(&project, "../sibling/file").is_err());
        assert!(SafePath::resolve(&project, "../../escape").is_err());
    }

    /// Windows case-insensitivity is required for `is_contained` to be usable at all;
    /// on Unix it would refuse legitimate paths. Both branches are exercised with the
    /// platform-appropriate expectation.
    #[test]
    fn case_handling_matches_platform_semantics() {
        let root_path = root();
        let flipped = if cfg!(windows) {
            root_path.to_string_lossy().to_uppercase()
        } else {
            root_path.to_string_lossy().replace("work", "WORK")
        };
        let flipped = PathBuf::from(flipped);
        if cfg!(windows) {
            assert!(
                SafePath::is_contained(&root_path, &flipped.join("src")),
                "Windows must compare path components case-insensitively"
            );
        } else {
            assert!(
                !SafePath::is_contained(&root_path, &flipped.join("src")),
                "Unix must compare path components case-sensitively"
            );
        }
    }

    #[test]
    fn display_is_stable() {
        assert_eq!(SafePath.to_string(), "SafePath");
    }
}
