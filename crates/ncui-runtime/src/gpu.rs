//! The GPU surface: what an agent can see and ask about GPU islands.
//!
//! ADR-002 makes two things observable: the `render_mode` of every component, "so a bad
//! DOM/GPU split is measurable rather than a matter of opinion", and the health of the
//! device, because "GPU device loss is a normal event, not an exceptional one, and
//! recovery is a tested path".
//!
//! So this surface reports those two things, and it reports them by asking the
//! implementation rather than by keeping a copy: the contract items come from
//! [`IslandHost::satisfied_contracts`], which drives a live host, and the device health
//! comes from the supervisor, which is the thing that rebuilds a lost device.
//!
//! # What is not here
//!
//! Drawing a frame into a window is the renderer's business, and the renderer's business
//! is `ncui-gpu-renderer`'s. This surface is the agent's view of the GPU: what islands
//! exist, how they are rendered, which of ADR-002's input items the host demonstrates, and
//! whether the device is alive.

use ncui_core::CapabilitySet;
use ncui_gpu::geometry::RenderMode;
use ncui_gpu::input::{InputContract, IslandHost, Row};
use serde_json::{Value, json};

/// The GPU surface a runtime exposes to agents.
#[derive(Debug, Default)]
pub struct Gpu {
    /// The island host, when one is installed.
    ///
    /// `None` until islands are installed, and a runtime with no islands has nothing to
    /// report about them — which is a different answer from "there are none".
    host: Option<IslandHost>,
    /// The device's adapter name, when a device has been created.
    adapter: Option<String>,
    /// How many times a lost device has been replaced.
    recoveries: u64,
    /// Whether the last observed device was reported lost.
    lost: bool,
}

impl Gpu {
    /// An empty surface.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs the island host.
    #[must_use]
    pub fn with_host(mut self, host: IslandHost) -> Self {
        self.host = Some(host);
        self
    }

    /// Whether a host is installed.
    #[must_use]
    pub const fn has_host(&self) -> bool {
        self.host.is_some()
    }

    /// The island host, when one is installed.
    #[must_use]
    pub const fn host(&self) -> Option<&IslandHost> {
        self.host.as_ref()
    }

    /// The contract items the host demonstrates.
    ///
    /// Empty when no host is installed: a runtime with no islands demonstrates nothing,
    /// and reporting the whole contract would be a claim about a host that is not there.
    #[must_use]
    pub fn satisfied_contracts(&self) -> Vec<InputContract> {
        self.host
            .as_ref()
            .map(IslandHost::satisfied_contracts)
            .unwrap_or_default()
    }

    /// The render mode of every island.
    #[must_use]
    pub fn render_modes(&self) -> Vec<(&str, RenderMode)> {
        self.host
            .as_ref()
            .map(IslandHost::render_modes)
            .unwrap_or_default()
    }

    /// Records the device's adapter, after one has been created.
    pub fn note_adapter(&mut self, adapter: impl Into<String>) {
        self.adapter = Some(adapter.into());
    }

    /// Records that a lost device was replaced.
    pub fn note_recovery(&mut self) {
        self.recoveries += 1;
        self.lost = false;
    }

    /// Records that the device was lost.
    pub fn note_loss(&mut self) {
        self.lost = true;
    }

    /// Whether the device is currently reported lost.
    #[must_use]
    pub const fn was_lost(&self) -> bool {
        self.lost
    }

    /// The agent-facing report.
    ///
    /// Every field is a fact an operator would otherwise have to ask a human for: which
    /// islands exist, how each is rendered, which of ADR-002's input items the host
    /// demonstrates, and what has happened to the device.
    #[must_use]
    pub fn inspect(&self) -> Value {
        let islands: Vec<Value> = self
            .render_modes()
            .into_iter()
            .map(|(id, mode)| json!({ "id": id, "render_mode": mode.as_str() }))
            .collect();
        let contracts: Vec<&str> = self
            .satisfied_contracts()
            .into_iter()
            .map(InputContract::as_str)
            .collect();
        json!({
            "host_installed": self.has_host(),
            "islands": islands,
            "satisfied_contracts": contracts,
            "adapter": self.adapter,
            "device_lost": self.lost,
            "device_recoveries": self.recoveries,
        })
    }
}

/// The capabilities a page needs to be told about the GPU surface.
///
/// A page is not an agent: it asks for actions, and the host maps them to methods. The
/// GPU's report is a read, so it needs the same authority an agent's read would.
#[must_use]
pub fn gpu_inspection_capability() -> CapabilitySet {
    CapabilitySet::empty().with(ncui_core::Capability::InspectGpu)
}

/// Builds a host over the given islands and rows.
#[must_use]
pub fn host_over(islands: ncui_gpu::geometry::Islands, rows: Vec<Row>) -> IslandHost {
    IslandHost::new(islands, rows)
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
    use ncui_gpu::geometry::{Island, Islands, LogicalSize, Rect, Viewport};

    fn host() -> IslandHost {
        let viewport = Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0);
        let mut islands = Islands::new(viewport);
        islands
            .add(
                Island::new("grid", Rect::new(0.0, 0.0, 1000.0, 600.0))
                    .with_content(LogicalSize::new(1000.0, 2000.0)),
            )
            .expect("added");
        let rows = (0..4)
            .map(|index| Row::new(&format!("row-{index}"), "text", 24.0))
            .collect();
        IslandHost::new(islands, rows)
    }

    #[test]
    fn an_empty_surface_reports_that_it_has_no_host() {
        let gpu = Gpu::new();
        assert!(!gpu.has_host());
        assert!(gpu.satisfied_contracts().is_empty());
        let report = gpu.inspect();
        assert_eq!(report["host_installed"], json!(false));
        assert_eq!(report["device_recoveries"], json!(0));
    }

    #[test]
    fn a_surface_with_a_host_reports_the_islands_render_modes() {
        let gpu = Gpu::new().with_host(host());
        assert!(gpu.has_host());
        let report = gpu.inspect();
        assert_eq!(report["host_installed"], json!(true));
        let islands = report["islands"].as_array().expect("an array");
        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0]["id"], json!("grid"));
        // An island is a GPU region by definition; the DOM mode is for a component that
        // is not one.
        assert_eq!(islands[0]["render_mode"], json!("gpu"));
    }

    #[test]
    fn a_device_loss_and_its_recovery_are_reported() {
        let mut gpu = Gpu::new().with_host(host());
        gpu.note_adapter("Test Adapter");
        gpu.note_loss();
        assert!(gpu.was_lost());
        let report = gpu.inspect();
        assert_eq!(report["device_lost"], json!(true));
        assert_eq!(report["adapter"], json!("Test Adapter"));

        gpu.note_recovery();
        let report = gpu.inspect();
        assert_eq!(report["device_lost"], json!(false));
        assert_eq!(report["device_recoveries"], json!(1));
    }

    #[test]
    fn the_contract_items_are_reported_by_name() {
        let gpu = Gpu::new().with_host(host());
        let report = gpu.inspect();
        let contracts = report["satisfied_contracts"].as_array().expect("an array");
        assert!(
            contracts.contains(&json!("mouse")),
            "the contract must be reported by name: {contracts:?}"
        );
        // The accessibility arm is answered by the semantic tree, and this host has none —
        // so it is absent, which is the honest answer rather than a default-true.
        assert!(
            !contracts.contains(&json!("accessibility")),
            "a host with no semantic tree must not report accessibility: {contracts:?}"
        );
    }

    #[test]
    fn a_host_with_a_semantic_tree_reports_accessibility() {
        let viewport = Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0);
        let mut islands = Islands::new(viewport);
        islands
            .add(
                Island::new("grid", Rect::new(0.0, 0.0, 1000.0, 600.0))
                    .with_content(LogicalSize::new(1000.0, 2000.0)),
            )
            .expect("added");
        let tree = ncui_a11y::SemanticTree::new().root(
            ncui_a11y::Node::with_children(ncui_a11y::Role::Window, "Grid", 0, false).child(
                ncui_a11y::Node::leaf(ncui_a11y::Role::Button, "Select all", 1, true),
            ),
        );
        let gpu = Gpu::new().with_host(IslandHost::new(islands, Vec::new()).with_tree(tree));
        let report = gpu.inspect();
        let contracts = report["satisfied_contracts"].as_array().expect("an array");
        assert!(
            contracts.contains(&json!("accessibility")),
            "a host with a tree must report accessibility: {contracts:?}"
        );
    }
}
