//! Global shortcuts: a key combination the platform delivers to the application.
//!
//! Every other way into an application goes through its window: the user finds it, focuses it, and
//! types. A global shortcut is the exception — the platform hands the combination to the application
//! whatever is in front — and it is the only way a user can reach an application they cannot see.
//! That is what makes it worth having, and it is also what makes it worth being careful about: a
//! shortcut is a key taken from every other program on the machine while the application runs.
//!
//! # What the platform does and what this does
//!
//! The platform owns the registration: it is the operating system that knows which application a
//! key belongs to, and it refuses a combination another application already holds. This module owns
//! the vocabulary — which accelerators an application may name, and what a refusal means — and the
//! bookkeeping that maps a registration back to the accelerator that asked for it.
//!
//! # Why the accelerator is parsed here
//!
//! The platform crate can parse a string too, but its accepted set is its own. Parsing here means
//! the vocabulary is stated in one place, refused by name, and tested: `Ctrl+Shift+K` is accepted,
//! `Ctrl+K+J` is not, `Hyper+X` is not, and a bare letter is not — because a bare key registered
//! globally is a key taken from every other application on the machine.
//!
//! # The dependency
//!
//! `global-hotkey` is ADR-012's sixth exception: the platform's own call is `RegisterHotKey`,
//! `XGrabKey` or `RegisterEventHotKey`, all foreign functions, and `unsafe_code = "forbid"` rejects
//! calling them. It is behind the `global-hotkeys` feature because on Linux it needs a connection to
//! an X server, which a headless machine does not have.

#[cfg(feature = "global-hotkeys")]
pub use enabled::{HotkeyError, Shortcut, Shortcuts, is_function_key, parse_accelerator};

#[cfg(feature = "global-hotkeys")]
mod enabled {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

    /// Why a shortcut could not be named, registered or removed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum HotkeyError {
        /// The accelerator names no key, or names something that is not a combination.
        NotACombination {
            /// What was given.
            accelerator: String,
        },
        /// A modifier this does not know.
        UnknownModifier {
            /// The word that was not a modifier.
            name: String,
        },
        /// A key this does not know.
        UnknownKey {
            /// The word that was not a key.
            name: String,
        },
        /// The same modifier twice.
        RepeatedModifier {
            /// The modifier that was repeated.
            name: String,
        },
        /// A bare key that is not a function key.
        BareKey {
            /// The key that was named without a modifier.
            name: String,
        },
        /// This machine cannot register shortcuts at all.
        Unavailable {
            /// What the platform said, when it said anything.
            reason: String,
        },
        /// The platform refused this combination — usually because another application holds it.
        Refused {
            /// The accelerator that was refused.
            accelerator: String,
            /// What the platform said.
            reason: String,
        },
        /// Nothing is registered under this accelerator.
        NotRegistered {
            /// The accelerator that was asked for.
            accelerator: String,
        },
    }

    impl core::fmt::Display for HotkeyError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::NotACombination { accelerator } => write!(
                    f,
                    "`{accelerator}` is not a shortcut: it must name one key, with modifiers \
                     joined by `+`, such as `Ctrl+Shift+K`"
                ),
                Self::UnknownModifier { name } => write!(
                    f,
                    "`{name}` is not a modifier: this accepts Ctrl, Shift, Alt and Super"
                ),
                Self::UnknownKey { name } => write!(f, "`{name}` is not a key this knows"),
                Self::RepeatedModifier { name } => {
                    write!(f, "`{name}` is named twice in one shortcut")
                }
                Self::BareKey { name } => write!(
                    f,
                    "`{name}` is a key with no modifier, and a bare key registered globally is a \
                     key taken from every other application on the machine; name a modifier, or \
                     use a function key"
                ),
                Self::Unavailable { reason } => {
                    write!(f, "this machine cannot register global shortcuts: {reason}")
                }
                Self::Refused {
                    accelerator,
                    reason,
                } => write!(
                    f,
                    "`{accelerator}` was refused by the platform: {reason}. Another application \
                     usually holds it"
                ),
                Self::NotRegistered { accelerator } => {
                    write!(f, "`{accelerator}` is not registered by this application")
                }
            }
        }
    }

    impl std::error::Error for HotkeyError {}

    /// One shortcut: the accelerator that named it, and the platform's own description of it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Shortcut {
        /// The accelerator, as the application wrote it, normalised.
        pub accelerator: String,
        /// What the platform registered.
        pub key: HotKey,
    }

    /// Whether `name` is a function key, which may be registered without a modifier.
    #[must_use]
    pub fn is_function_key(name: &str) -> bool {
        let upper = name.to_ascii_uppercase();
        let Some(number) = upper.strip_prefix('F') else {
            return false;
        };
        number
            .parse::<u8>()
            .is_ok_and(|number| (1..=24).contains(&number))
    }

    /// Reads an accelerator: modifiers joined by `+`, then one key.
    ///
    /// # Errors
    ///
    /// Returns [`HotkeyError`] naming what was wrong: a word that is not a modifier, a word that is
    /// not a key, the same modifier twice, a bare key that is not a function key, or an accelerator
    /// that names no key at all.
    pub fn parse_accelerator(accelerator: &str) -> Result<Shortcut, HotkeyError> {
        let trimmed = accelerator.trim();
        let mut parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        let Some(key_name) = parts.pop() else {
            return Err(HotkeyError::NotACombination {
                accelerator: accelerator.to_owned(),
            });
        };
        if parts.is_empty() && key_name.is_empty() {
            return Err(HotkeyError::NotACombination {
                accelerator: accelerator.to_owned(),
            });
        }
        let modifiers = read_modifiers(&parts, accelerator)?;
        if key_name.is_empty() {
            return Err(HotkeyError::NotACombination {
                accelerator: accelerator.to_owned(),
            });
        }
        let code = code_for(key_name).ok_or_else(|| HotkeyError::UnknownKey {
            name: key_name.to_owned(),
        })?;
        if modifiers.is_none() && !is_function_key(key_name) {
            return Err(HotkeyError::BareKey {
                name: key_name.to_owned(),
            });
        }
        Ok(Shortcut {
            accelerator: normalise(&parts, key_name),
            key: HotKey::new(modifiers, code),
        })
    }

    /// The accelerator rebuilt from what was accepted.
    ///
    /// Built from the accepted words rather than kept as the caller wrote them, so what is stored
    /// and reported is this vocabulary's own spelling whatever case the caller happened to use.
    fn normalise(parts: &[&str], key_name: &str) -> String {
        let mut normalised = String::new();
        for part in parts {
            if !normalised.is_empty() {
                normalised.push('+');
            }
            normalised.push_str(&canonical_modifier(part));
        }
        if !normalised.is_empty() {
            normalised.push('+');
        }
        normalised.push_str(&canonical_key(key_name));
        normalised
    }

    /// Reads the modifiers named before the key.
    ///
    /// # Errors
    ///
    /// Returns [`HotkeyError::UnknownModifier`] for a word that is not one, and
    /// [`HotkeyError::RepeatedModifier`] for the same one twice — naming `Ctrl` twice is a
    /// mistake rather than a way to ask for something stronger.
    fn read_modifiers(parts: &[&str], accelerator: &str) -> Result<Option<Modifiers>, HotkeyError> {
        let mut modifiers: Option<Modifiers> = None;
        for part in parts {
            if part.is_empty() {
                return Err(HotkeyError::NotACombination {
                    accelerator: accelerator.to_owned(),
                });
            }
            let flag = modifier_flag(part)?;
            match modifiers {
                Some(existing) if existing.contains(flag) => {
                    return Err(HotkeyError::RepeatedModifier {
                        name: (*part).to_owned(),
                    });
                }
                Some(existing) => modifiers = Some(existing | flag),
                None => modifiers = Some(flag),
            }
        }
        Ok(modifiers)
    }

    /// The platform's flag for one modifier word.
    fn modifier_flag(part: &str) -> Result<Modifiers, HotkeyError> {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Ok(Modifiers::CONTROL),
            "shift" => Ok(Modifiers::SHIFT),
            "alt" => Ok(Modifiers::ALT),
            "super" | "meta" | "cmd" | "command" | "win" => Ok(Modifiers::META),
            _ => Err(HotkeyError::UnknownModifier {
                name: part.to_owned(),
            }),
        }
    }

    /// The platform's code for a key written as one letter or one digit.
    ///
    /// Separate from the named keys because a letter or a digit is one character and everything
    /// else is a word: the two are read differently and refusing one is not refusing the other.
    fn single_character_code(name: &str) -> Option<Code> {
        let upper = name.to_ascii_uppercase();
        if upper.len() != 1 {
            return None;
        }
        let character = upper.chars().next()?;
        if character.is_ascii_uppercase() {
            return Some(match character {
                'A' => Code::KeyA,
                'B' => Code::KeyB,
                'C' => Code::KeyC,
                'D' => Code::KeyD,
                'E' => Code::KeyE,
                'F' => Code::KeyF,
                'G' => Code::KeyG,
                'H' => Code::KeyH,
                'I' => Code::KeyI,
                'J' => Code::KeyJ,
                'K' => Code::KeyK,
                'L' => Code::KeyL,
                'M' => Code::KeyM,
                'N' => Code::KeyN,
                'O' => Code::KeyO,
                'P' => Code::KeyP,
                'Q' => Code::KeyQ,
                'R' => Code::KeyR,
                'S' => Code::KeyS,
                'T' => Code::KeyT,
                'U' => Code::KeyU,
                'V' => Code::KeyV,
                'W' => Code::KeyW,
                'X' => Code::KeyX,
                'Y' => Code::KeyY,
                _ => Code::KeyZ,
            });
        }
        if character.is_ascii_digit() {
            return Some(match character {
                '0' => Code::Digit0,
                '1' => Code::Digit1,
                '2' => Code::Digit2,
                '3' => Code::Digit3,
                '4' => Code::Digit4,
                '5' => Code::Digit5,
                '6' => Code::Digit6,
                '7' => Code::Digit7,
                '8' => Code::Digit8,
                _ => Code::Digit9,
            });
        }
        None
    }

    /// The spelling this vocabulary uses for a modifier.
    fn canonical_modifier(part: &str) -> String {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "Ctrl".to_owned(),
            "shift" => "Shift".to_owned(),
            "alt" => "Alt".to_owned(),
            _ => "Super".to_owned(),
        }
    }

    /// The spelling this vocabulary uses for a key.
    fn canonical_key(name: &str) -> String {
        let upper = name.to_ascii_uppercase();
        if upper.len() == 1 && upper.chars().all(|c| c.is_ascii_alphanumeric()) {
            return upper;
        }
        match upper.as_str() {
            "SPACE" => "Space".to_owned(),
            "TAB" => "Tab".to_owned(),
            "ENTER" | "RETURN" => "Enter".to_owned(),
            "ESCAPE" | "ESC" => "Escape".to_owned(),
            "BACKSPACE" => "Backspace".to_owned(),
            "DELETE" | "DEL" => "Delete".to_owned(),
            "INSERT" | "INS" => "Insert".to_owned(),
            "HOME" => "Home".to_owned(),
            "END" => "End".to_owned(),
            "PAGEUP" | "PGUP" => "PageUp".to_owned(),
            "PAGEDOWN" | "PGDN" => "PageDown".to_owned(),
            "UP" | "ARROWUP" => "Up".to_owned(),
            "DOWN" | "ARROWDOWN" => "Down".to_owned(),
            "LEFT" | "ARROWLEFT" => "Left".to_owned(),
            "RIGHT" | "ARROWRIGHT" => "Right".to_owned(),
            "MINUS" => "Minus".to_owned(),
            "EQUAL" | "PLUS" => "Equal".to_owned(),
            "COMMA" => "Comma".to_owned(),
            "PERIOD" | "DOT" => "Period".to_owned(),
            "SLASH" => "Slash".to_owned(),
            "SEMICOLON" => "Semicolon".to_owned(),
            "QUOTE" | "APOSTROPHE" => "Quote".to_owned(),
            "BRACKETLEFT" => "BracketLeft".to_owned(),
            "BRACKETRIGHT" => "BracketRight".to_owned(),
            "BACKQUOTE" | "GRAVE" => "Backquote".to_owned(),
            "BACKSLASH" => "Backslash".to_owned(),
            other => {
                // A function key, which is the only remaining shape this accepts.
                format!("F{}", other.trim_start_matches('F'))
            }
        }
    }

    /// The platform's code for a key this vocabulary accepts.
    fn code_for(name: &str) -> Option<Code> {
        if let Some(code) = single_character_code(name) {
            return Some(code);
        }
        Some(match name.to_ascii_uppercase().as_str() {
            "SPACE" => Code::Space,
            "TAB" => Code::Tab,
            "ENTER" | "RETURN" => Code::Enter,
            "ESCAPE" | "ESC" => Code::Escape,
            "BACKSPACE" => Code::Backspace,
            "DELETE" | "DEL" => Code::Delete,
            "INSERT" | "INS" => Code::Insert,
            "HOME" => Code::Home,
            "END" => Code::End,
            "PAGEUP" | "PGUP" => Code::PageUp,
            "PAGEDOWN" | "PGDN" => Code::PageDown,
            "UP" | "ARROWUP" => Code::ArrowUp,
            "DOWN" | "ARROWDOWN" => Code::ArrowDown,
            "LEFT" | "ARROWLEFT" => Code::ArrowLeft,
            "RIGHT" | "ARROWRIGHT" => Code::ArrowRight,
            "MINUS" => Code::Minus,
            "EQUAL" | "PLUS" => Code::Equal,
            "COMMA" => Code::Comma,
            "PERIOD" | "DOT" => Code::Period,
            "SLASH" => Code::Slash,
            "SEMICOLON" => Code::Semicolon,
            "QUOTE" | "APOSTROPHE" => Code::Quote,
            "BRACKETLEFT" => Code::BracketLeft,
            "BRACKETRIGHT" => Code::BracketRight,
            "BACKQUOTE" | "GRAVE" => Code::Backquote,
            "BACKSLASH" => Code::Backslash,
            "F1" => Code::F1,
            "F2" => Code::F2,
            "F3" => Code::F3,
            "F4" => Code::F4,
            "F5" => Code::F5,
            "F6" => Code::F6,
            "F7" => Code::F7,
            "F8" => Code::F8,
            "F9" => Code::F9,
            "F10" => Code::F10,
            "F11" => Code::F11,
            "F12" => Code::F12,
            "F13" => Code::F13,
            "F14" => Code::F14,
            "F15" => Code::F15,
            "F16" => Code::F16,
            "F17" => Code::F17,
            "F18" => Code::F18,
            "F19" => Code::F19,
            "F20" => Code::F20,
            "F21" => Code::F21,
            "F22" => Code::F22,
            "F23" => Code::F23,
            "F24" => Code::F24,
            _ => return None,
        })
    }

    /// The shortcuts an application has asked the platform for.
    ///
    /// One of these per application: the platform's channel of presses is process-wide, so a second
    /// manager would receive events it did not register for.
    pub struct Shortcuts {
        /// The platform's registration table.
        manager: GlobalHotKeyManager,
        /// What was registered, by the platform's id for it.
        registered: Mutex<BTreeMap<u32, Shortcut>>,
        /// The next id to hand out, so two registrations cannot share one.
        next_id: AtomicU32,
    }

    impl core::fmt::Debug for Shortcuts {
        /// Reports what is held rather than the platform's handle: a registration table is not a
        /// value a reader can learn anything from, and what matters is which keys are taken.
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Shortcuts")
                .field("registered", &self.registered())
                .finish_non_exhaustive()
        }
    }

    impl Shortcuts {
        /// Asks the platform for a registration table.
        ///
        /// # Errors
        ///
        /// Returns [`HotkeyError::Unavailable`] when this machine cannot register shortcuts at all —
        /// a session with no display server, for instance. That is a fact about the machine rather
        /// than a mistake in the request, and it is reported as such.
        pub fn new() -> Result<Self, HotkeyError> {
            let manager = GlobalHotKeyManager::new().map_err(|error| HotkeyError::Unavailable {
                reason: error.to_string(),
            })?;
            Ok(Self {
                manager,
                registered: Mutex::new(BTreeMap::new()),
                next_id: AtomicU32::new(1),
            })
        }

        /// Registers `accelerator`, replacing nothing.
        ///
        /// # Errors
        ///
        /// Returns what [`parse_accelerator`] reports for an accelerator this does not accept,
        /// [`HotkeyError::Refused`] when the platform refuses the combination, and
        /// [`HotkeyError::Refused`] when this application already holds it.
        pub fn register(&self, accelerator: &str) -> Result<Shortcut, HotkeyError> {
            let mut shortcut = parse_accelerator(accelerator)?;
            {
                let registered = self
                    .registered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if registered
                    .values()
                    .any(|held| held.accelerator == shortcut.accelerator)
                {
                    return Err(HotkeyError::Refused {
                        accelerator: shortcut.accelerator,
                        reason: "this application already holds it".to_owned(),
                    });
                }
            }
            shortcut.key.id = self.next_id.fetch_add(1, Ordering::Relaxed);
            self.manager
                .register(shortcut.key)
                .map_err(|error| HotkeyError::Refused {
                    accelerator: shortcut.accelerator.clone(),
                    reason: error.to_string(),
                })?;
            self.registered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(shortcut.key.id, shortcut.clone());
            Ok(shortcut)
        }

        /// Removes the shortcut `accelerator` names.
        ///
        /// # Errors
        ///
        /// Returns [`HotkeyError::NotRegistered`] when this application does not hold it.
        pub fn unregister(&self, accelerator: &str) -> Result<(), HotkeyError> {
            let wanted = parse_accelerator(accelerator)?;
            let mut registered = self
                .registered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some((id, shortcut)) = registered
                .iter()
                .find(|(_, held)| held.accelerator == wanted.accelerator)
                .map(|(id, held)| (*id, held.clone()))
            else {
                return Err(HotkeyError::NotRegistered {
                    accelerator: wanted.accelerator,
                });
            };
            self.manager
                .unregister(shortcut.key)
                .map_err(|error| HotkeyError::Refused {
                    accelerator: shortcut.accelerator.clone(),
                    reason: error.to_string(),
                })?;
            registered.remove(&id);
            Ok(())
        }

        /// The accelerators this application holds, in the order they were registered.
        #[must_use]
        pub fn registered(&self) -> Vec<String> {
            let registered = self
                .registered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut held: Vec<(u32, String)> = registered
                .iter()
                .map(|(id, shortcut)| (*id, shortcut.accelerator.clone()))
                .collect();
            held.sort_by_key(|(id, _)| *id);
            held.into_iter().map(|(_, name)| name).collect()
        }

        /// The accelerator of a shortcut the user has pressed, if one is waiting.
        ///
        /// A press is taken once: the platform's channel is process-wide, and an application that
        /// left events in it would deliver each press again on its next turn.
        #[must_use]
        pub fn take_pressed(&self) -> Option<String> {
            let receiver = GlobalHotKeyEvent::receiver();
            while let Ok(event) = receiver.try_recv() {
                if event.state != HotKeyState::Pressed {
                    continue;
                }
                let registered = self
                    .registered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(shortcut) = registered.get(&event.id) {
                    return Some(shortcut.accelerator.clone());
                }
            }
            None
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
        use super::*;

        #[test]
        fn an_accelerator_is_read_into_a_modifier_set_and_a_key() {
            let shortcut = parse_accelerator("Ctrl+Shift+K").expect("a shortcut");
            assert_eq!(shortcut.accelerator, "Ctrl+Shift+K");
            assert_eq!(shortcut.key.key, Code::KeyK);
            assert!(shortcut.key.mods.contains(Modifiers::CONTROL));
            assert!(shortcut.key.mods.contains(Modifiers::SHIFT));

            // The spelling is the vocabulary's own, whatever the caller wrote.
            let lower = parse_accelerator("ctrl+shift+k").expect("a shortcut");
            assert_eq!(lower.accelerator, "Ctrl+Shift+K");
            assert_eq!(lower.key.key, shortcut.key.key);
            let aliases = parse_accelerator("Control+Meta+K").expect("a shortcut");
            assert_eq!(aliases.accelerator, "Ctrl+Super+K");
        }

        #[test]
        fn a_shortcut_this_does_not_accept_is_refused_by_name() {
            assert_eq!(
                parse_accelerator(""),
                Err(HotkeyError::NotACombination {
                    accelerator: String::new()
                })
            );
            assert_eq!(
                parse_accelerator("Ctrl+"),
                Err(HotkeyError::NotACombination {
                    accelerator: "Ctrl+".to_owned()
                })
            );
            assert_eq!(
                parse_accelerator("Hyper+K"),
                Err(HotkeyError::UnknownModifier {
                    name: "Hyper".to_owned()
                })
            );
            assert_eq!(
                parse_accelerator("Ctrl+Banana"),
                Err(HotkeyError::UnknownKey {
                    name: "Banana".to_owned()
                })
            );
            assert_eq!(
                parse_accelerator("Ctrl+Ctrl+K"),
                Err(HotkeyError::RepeatedModifier {
                    name: "Ctrl".to_owned()
                })
            );
            // Two keys and one modifier: the last word is read as the key and everything before it
            // as a modifier, so `K` is refused as a modifier rather than silently dropped.
            assert_eq!(
                parse_accelerator("Ctrl+K+J"),
                Err(HotkeyError::UnknownModifier {
                    name: "K".to_owned()
                })
            );
        }

        #[test]
        fn a_bare_key_is_refused_unless_it_is_a_function_key() {
            // The defect this prevents: a bare key registered globally is a key taken from every
            // other application on the machine.
            assert_eq!(
                parse_accelerator("K"),
                Err(HotkeyError::BareKey {
                    name: "K".to_owned()
                })
            );
            assert_eq!(
                parse_accelerator("Space"),
                Err(HotkeyError::BareKey {
                    name: "Space".to_owned()
                })
            );
            // Function keys are the exception: they are the keys a machine has to spare.
            let f = parse_accelerator("F9").expect("a function key alone is a shortcut");
            assert_eq!(f.key.key, Code::F9);
            assert!(f.key.mods.is_empty());
            assert!(is_function_key("F24"));
            assert!(!is_function_key("F25"));
            assert!(!is_function_key("K"));
        }

        #[test]
        fn the_vocabulary_names_what_a_shortcut_can_use() {
            // Every name this accepts must map to a distinct key, and a name it does not accept
            // must be refused rather than mapped to something close to it.
            for (name, expected) in [
                ("A", Code::KeyA),
                ("Z", Code::KeyZ),
                ("0", Code::Digit0),
                ("9", Code::Digit9),
                ("F1", Code::F1),
                ("F24", Code::F24),
                ("Space", Code::Space),
                ("Tab", Code::Tab),
                ("Enter", Code::Enter),
                ("Escape", Code::Escape),
                ("Up", Code::ArrowUp),
                ("Left", Code::ArrowLeft),
                ("PageDown", Code::PageDown),
                ("Backquote", Code::Backquote),
                ("Backslash", Code::Backslash),
            ] {
                assert_eq!(code_for(name), Some(expected), "{name}");
            }
            for name in ["", "F25", "F0", "Numpad5", "PrintScreen", "K+", "aa"] {
                assert_eq!(code_for(name), None, "`{name}` must not be a key");
            }
        }

        #[test]
        fn a_machine_that_cannot_register_shortcuts_says_so() {
            // A session with no display server cannot hold a shortcut, and the refusal must be a
            // value rather than a panic. On a machine that can, the registration table is empty
            // until something is registered.
            match Shortcuts::new() {
                Ok(shortcuts) => assert!(shortcuts.registered().is_empty()),
                Err(error) => assert!(
                    matches!(error, HotkeyError::Unavailable { .. }),
                    "got {error}"
                ),
            }
        }

        #[test]
        fn a_registered_shortcut_is_held_and_can_be_released() {
            let Ok(shortcuts) = Shortcuts::new() else {
                return;
            };
            // A combination no application is likely to hold. If the platform refuses it anyway,
            // the refusal is the platform's and is reported as such.
            let Ok(held) = shortcuts.register("Ctrl+Alt+F13") else {
                return;
            };
            assert_eq!(held.accelerator, "Ctrl+Alt+F13");
            assert_eq!(shortcuts.registered(), vec!["Ctrl+Alt+F13".to_owned()]);

            // The same combination twice is refused by this application rather than registered
            // twice: two registrations would give one press two answers.
            assert!(matches!(
                shortcuts.register("Ctrl+Alt+F13"),
                Err(HotkeyError::Refused { .. })
            ));

            shortcuts
                .unregister("Ctrl+Alt+F13")
                .expect("the shortcut is released");
            assert!(shortcuts.registered().is_empty());
            assert_eq!(
                shortcuts.unregister("Ctrl+Alt+F13"),
                Err(HotkeyError::NotRegistered {
                    accelerator: "Ctrl+Alt+F13".to_owned()
                })
            );
        }
    }
}
