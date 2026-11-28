//! Browser/WebGPU target shell (ADR-021): the ncui-native rendering engine
//! running inside a real browser, drawing through WebGPU.
//!
//! The browser is the platform here the way Win32 is the platform on Windows:
//! the shell owns canvas sizing (`ResizeObserver`, `devicePixelRatio`), frame
//! scheduling (requestAnimationFrame), DOM event input, clipboard, text
//! services, and the accessibility mirror. On non-wasm targets the crate is
//! empty — the shell is definitionally browser-only, and compiling it natively
//! would be the compile-only fallacy this crate exists to avoid.

#[cfg(target_arch = "wasm32")]
mod shell;

#[cfg(target_arch = "wasm32")]
pub use shell::*;
