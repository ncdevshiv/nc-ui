//! The bounds a component is executed under.
//!
//! Every field is a limit the host can be *held to*, and every one of them is enforced in
//! [`crate::host`] rather than documented and hoped for. The defaults are chosen so a
//! well-behaved component never notices them and a hostile one cannot use them as a resource:
//! they bound what a component may consume, not what it may achieve.
//!
//! A zero in any field is a refusal to configure rather than an unlimited allowance, which is
//! why [`ComponentLimits::validate`] runs before the engine is built.

use crate::error::ComponentError;

/// A page of WebAssembly linear memory, in bytes. Fixed by the specification.
pub const WASM_PAGE_BYTES: usize = 64 * 1024;

/// What the host will let one component cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentLimits {
    /// Largest artifact the host will read from disk or from a request body.
    pub max_module_bytes: usize,
    /// Largest manifest document the host will parse out of the artifact.
    pub max_manifest_bytes: usize,
    /// Highest the guest's memory may grow, in bytes. Not pages: `StoreLimits` is in bytes and
    /// a page-based field next to a byte-based enforcement point would be an off-by-64 KiB bug
    /// waiting to be made.
    pub max_memory_bytes: usize,
    /// Fuel one invocation may consume before it is trapped.
    pub fuel_per_invoke: u64,
    /// Largest request body the host will write into a component's scratch region.
    pub max_input_bytes: usize,
    /// Largest response the host will read back out of the component's memory.
    pub max_output_bytes: usize,
    /// Largest single copy the host will perform out of guest memory on the component's behalf.
    pub max_guest_copy_bytes: usize,
    /// Longest method or service name the host will accept.
    pub max_name_bytes: usize,
}

impl Default for ComponentLimits {
    fn default() -> Self {
        Self {
            // A component that needs 8 MiB of code is not a plugin, it is an application, and
            // it should ship as one.
            max_module_bytes: 8 * 1024 * 1024,
            max_manifest_bytes: 256 * 1024,
            // 256 pages. Enough for a real JSON workload, small enough that a component that
            // allocates until the user's machine stops is a component that ran out of memory.
            max_memory_bytes: 16 * 1024 * 1024,
            // Roughly one instruction per unit. Twenty million is far past any sane single
            // call and well short of the time a user would consider acceptable.
            fuel_per_invoke: 20_000_000,
            max_input_bytes: 1024 * 1024,
            max_output_bytes: 1024 * 1024,
            max_guest_copy_bytes: 1024 * 1024,
            max_name_bytes: 256,
        }
    }
}

impl ComponentLimits {
    /// Rejects a configuration that would not bound anything.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentError::InvalidLimits`] naming the first field that is zero, and
    /// [`ComponentError::InconsistentLimits`] when the scratch budget cannot hold the largest
    /// document the host is willing to write.
    pub fn validate(&self) -> Result<(), ComponentError> {
        let zero = [
            ("max_module_bytes", self.max_module_bytes),
            ("max_manifest_bytes", self.max_manifest_bytes),
            ("max_memory_bytes", self.max_memory_bytes),
            (
                "fuel_per_invoke",
                usize::try_from(self.fuel_per_invoke).unwrap_or(usize::MAX),
            ),
            ("max_input_bytes", self.max_input_bytes),
            ("max_output_bytes", self.max_output_bytes),
            ("max_guest_copy_bytes", self.max_guest_copy_bytes),
            ("max_name_bytes", self.max_name_bytes),
        ];
        if let Some((field, _)) = zero.iter().find(|(_, value)| *value == 0) {
            return Err(ComponentError::InvalidLimits((*field).to_owned()));
        }
        // A manifest read is a host call into the component's own memory; if the copy budget
        // were smaller than the manifest bound, the host could never read a manifest it was
        // willing to accept.
        if self.max_guest_copy_bytes < self.max_manifest_bytes {
            return Err(ComponentError::InconsistentLimits(
                "the guest copy budget is smaller than the manifest bound, so a manifest the \
                 host would accept could never be read"
                    .to_owned(),
            ));
        }
        // The same trap one field further: an input the host would write but cannot fit in
        // memory at all is a component that can never be called.
        if self.max_input_bytes + self.max_name_bytes > self.max_memory_bytes {
            return Err(ComponentError::InconsistentLimits(
                "memory is capped below the largest request the host would write".to_owned(),
            ));
        }
        if !self.max_memory_bytes.is_multiple_of(WASM_PAGE_BYTES) {
            return Err(ComponentError::InconsistentLimits(format!(
                "the memory cap must be a whole number of {} KiB pages",
                WASM_PAGE_BYTES / 1024
            )));
        }
        Ok(())
    }

    /// The memory cap expressed in pages, for the engine's own bounds.
    #[must_use]
    pub const fn max_memory_pages(&self) -> u64 {
        (self.max_memory_bytes / WASM_PAGE_BYTES) as u64
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
    fn the_defaults_are_valid() {
        ComponentLimits::default()
            .validate()
            .expect("usable defaults");
    }

    #[test]
    fn a_zero_bound_is_a_refusal_not_an_unlimited_allowance() {
        let limits = ComponentLimits {
            max_module_bytes: 0,
            ..ComponentLimits::default()
        };
        let error = limits.validate().expect_err("zero is not unlimited");
        assert_eq!(
            error,
            ComponentError::InvalidLimits("max_module_bytes".to_owned())
        );

        let limits = ComponentLimits {
            fuel_per_invoke: 0,
            ..ComponentLimits::default()
        };
        assert!(
            matches!(limits.validate(), Err(ComponentError::InvalidLimits(field)) if field == "fuel_per_invoke")
        );
    }

    #[test]
    fn a_copy_budget_smaller_than_the_manifest_bound_is_refused() {
        let limits = ComponentLimits {
            max_guest_copy_bytes: 1024,
            ..ComponentLimits::default()
        };
        assert!(
            matches!(
                limits.validate(),
                Err(ComponentError::InconsistentLimits(_))
            ),
            "the host could never read a manifest it claims to accept"
        );
    }

    #[test]
    fn a_memory_cap_below_the_largest_request_is_refused() {
        let limits = ComponentLimits {
            max_memory_bytes: WASM_PAGE_BYTES,
            max_input_bytes: 4 * 1024 * 1024,
            ..ComponentLimits::default()
        };
        assert!(matches!(
            limits.validate(),
            Err(ComponentError::InconsistentLimits(_))
        ));
    }

    #[test]
    fn a_memory_cap_that_is_not_whole_pages_is_refused() {
        let limits = ComponentLimits {
            max_memory_bytes: WASM_PAGE_BYTES + 1,
            ..ComponentLimits::default()
        };
        assert!(matches!(
            limits.validate(),
            Err(ComponentError::InconsistentLimits(_))
        ));
    }

    #[test]
    fn the_page_conversion_is_the_one_the_engine_expects() {
        let limits = ComponentLimits {
            max_memory_bytes: 4 * WASM_PAGE_BYTES,
            ..ComponentLimits::default()
        };
        assert_eq!(limits.max_memory_pages(), 4);
    }
}
