//! Editor discovery: which programs on this machine can open a file, and where they are.
//!
//! "Open this file in my editor" is a question about the machine, not the application: the
//! runtime does not bundle an editor, and the honest answer to "which editors are here?" is
//! a lookup over the machine's own search path, in the platform's own order. This module
//! holds the catalogue of editors it knows how to name and the lookup that finds them.
//!
//! # What the lookup covers
//!
//! The catalogue is a fixed list of editors whose launch programs are conventionally on the
//! search path (`code`, `subl`, `nvim`, …). An editor whose installer does not put it on the
//! search path is not discovered by this module — claiming a discovery that did not happen
//! would be worse than not listing it. The search paths are supplied by the caller, which is
//! what makes the lookup testable without touching the machine's environment.
//!
//! The first directory on the search path that holds a program wins for that editor, because
//! that is the one the platform would run.

use std::path::{Path, PathBuf};

/// The editors this module can discover, in the order they are reported.
pub const EDITORS: &[Editor] = &[
    Editor {
        name: "VS Code",
        program: "code",
    },
    Editor {
        name: "VSCodium",
        program: "codium",
    },
    Editor {
        name: "Sublime Text",
        program: "subl",
    },
    Editor {
        name: "Notepad++",
        program: "notepad++",
    },
    Editor {
        name: "Neovim",
        program: "nvim",
    },
    Editor {
        name: "Vim",
        program: "vim",
    },
    Editor {
        name: "Emacs",
        program: "emacs",
    },
    Editor {
        name: "Helix",
        program: "hx",
    },
    Editor {
        name: "gedit",
        program: "gedit",
    },
    Editor {
        name: "Notepad",
        program: "notepad",
    },
];

/// One editor this module knows how to look for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Editor {
    /// The name shown to a person, e.g. `"VS Code"`.
    pub name: &'static str,
    /// The launch program the editor is conventionally reachable as.
    pub program: &'static str,
}

/// One editor that was found on the machine, and where.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiscoveredEditor {
    /// The editor's display name.
    pub name: String,
    /// The launch program.
    pub program: String,
    /// The program file that was found.
    pub path: PathBuf,
}

/// Splits a `PATH`-shaped environment value into its directories.
///
/// Empty entries are dropped: they carry no directory, and a lookup over them would be a
/// lookup over nothing.
#[must_use]
pub fn parse_search_path(path_env: &str) -> Vec<PathBuf> {
    path_env
        .split(std::path::MAIN_SEPARATOR_STR)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Finds the known editors reachable from `search_paths`, in catalogue order.
///
/// An editor is reported once — for the first search path that holds its program — and
/// only when the program file actually exists there. The paths are supplied rather than
/// read, so a caller that wants the machine's own answer passes its `PATH` entries, and a
/// test passes directories it owns.
#[must_use]
pub fn discover(search_paths: &[PathBuf]) -> Vec<DiscoveredEditor> {
    let mut found = Vec::new();
    for editor in EDITORS {
        if let Some(path) = locate(editor.program, search_paths) {
            found.push(DiscoveredEditor {
                name: editor.name.to_owned(),
                program: editor.program.to_owned(),
                path,
            });
        }
    }
    found
}

/// Whether `program` can be opened by name from `search_paths`, and where.
fn locate(program: &str, search_paths: &[PathBuf]) -> Option<PathBuf> {
    for directory in search_paths {
        let candidate = directory.join(program).with_exe();
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Appends the platform's executable suffix, so a lookup on Windows finds `code.exe`.
trait WithExe {
    fn with_exe(&self) -> PathBuf;
}

impl WithExe for Path {
    fn with_exe(&self) -> PathBuf {
        if cfg!(windows) {
            let mut path = self.as_os_str().to_os_string();
            path.push(".exe");
            PathBuf::from(path)
        } else {
            self.to_path_buf()
        }
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::{discover, parse_search_path};
    use std::path::PathBuf;

    /// A directory this test owns, holding fixture programs.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("ncui-editor-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the scratch is creatable");
            Self { path }
        }

        /// Writes a fixture program, with the executable suffix on Windows.
        fn install(&self, program: &str) -> PathBuf {
            let file = if cfg!(windows) {
                self.path.join(format!("{program}.exe"))
            } else {
                self.path.join(program)
            };
            std::fs::write(&file, b"").expect("the fixture is written");
            file
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn a_search_path_splits_into_directories_and_drops_the_empty_entries() {
        let separator = std::path::MAIN_SEPARATOR_STR;
        let path = format!("a{separator}{separator}b{separator}");
        let parsed = parse_search_path(&path);
        assert_eq!(parsed, vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert!(parse_search_path("").is_empty());
    }

    #[test]
    fn discovery_finds_the_editors_the_paths_hold_and_no_others() {
        let scratch = Scratch::new("found");
        scratch.install("code");
        scratch.install("nvim");

        let found = discover(std::slice::from_ref(&scratch.path));
        let names: Vec<&str> = found.iter().map(|editor| editor.name.as_str()).collect();
        assert_eq!(names, vec!["VS Code", "Neovim"], "{found:?}");
        assert!(
            found[0].path.is_file() && found[0].path.starts_with(&scratch.path),
            "the discovered path is the fixture program"
        );

        assert!(
            discover(&[PathBuf::from("Z:/nowhere")]).is_empty(),
            "no paths, no editors"
        );
    }

    #[test]
    fn the_first_path_that_holds_a_program_wins_for_that_editor() {
        let first = Scratch::new("first");
        let second = Scratch::new("second");
        first.install("vim");
        second.install("vim");
        second.install("code");

        let found = discover(&[first.path.clone(), second.path.clone()]);
        let vim = found
            .iter()
            .find(|editor| editor.program == "vim")
            .expect("vim is on the first path");
        assert!(
            vim.path.starts_with(&first.path),
            "the first path wins for vim: {vim:?}"
        );
        let code = found
            .iter()
            .find(|editor| editor.program == "code")
            .expect("code is on the second path");
        assert!(code.path.starts_with(&second.path), "{code:?}");
    }
}
