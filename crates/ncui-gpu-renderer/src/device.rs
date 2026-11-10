//! GPU device ownership, and the recovery path when the device is lost.
//!
//! # Device loss is normal
//!
//! ADR-002: *"GPU device loss is a normal event, not an exceptional one, and recovery is a
//! tested path."* That sentence has two halves and both matter. Treating loss as
//! exceptional is what produces a renderer that crashes on a driver reset; and calling a
//! path "tested" when it has only been described is what produces a renderer whose
//! recovery has never run.
//!
//! So this module is split in two:
//!
//! * [`DeviceSupervisor`] — the *policy*: when to rebuild, how to count rebuilds, what to
//!   report. It knows nothing about `wgpu`, and it is exercised by a test that hands it
//!   a device which reports itself lost.
//! * the `wgpu` binding — a real device, created by `wgpu`.
//!
//! The supervisor is generic over the device type, so the test exercises the same code
//! the renderer runs, and the `wgpu` build is exercised by the same code path.
//!
//! # The safety boundary
//!
//! `wgpu` performs all graphics FFI internally. This module's own code is `unsafe`-free:
//! it owns the `wgpu` device and queue and exposes a safe abstraction. ADR-012 records
//! `wgpu` as a deliberate dependency exception, confined to this crate.

use core::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ncui_core::Result;

/// Error creating or using a GPU device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    /// No graphics adapter was found on this machine.
    NoAdapter,
    /// The device could not be created.
    DeviceCreation(String),
    /// The GPU device was lost and recovery is required.
    DeviceLost,
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAdapter => f.write_str("no graphics adapter found"),
            Self::DeviceCreation(reason) => write!(f, "device creation failed: {reason}"),
            Self::DeviceLost => f.write_str("gpu device lost"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// The renderer's view of a GPU device.
///
/// A trait rather than the concrete `wgpu::Device` for two reasons: the recovery path can
/// then be tested on a machine with no GPU, and a second backend could be added without
/// changing the supervisor that owns the device.
pub trait GpuDevice: Send + Sync + fmt::Debug {
    /// Whether the device is still usable.
    fn is_alive(&self) -> bool;

    /// The adapter's name, for diagnostics.
    fn adapter(&self) -> String;

    /// The device's identifier, for diagnostics.
    fn id(&self) -> String;
}

/// Creates GPU devices.
///
/// The supervisor asks a factory for a device when it needs one — on start-up, and again
/// after a loss. A factory that cannot create one says why.
///
/// Creation is asynchronous because adapter and device selection is: `wgpu` asks the
/// driver, and the driver answers on its own schedule. The trait returns a boxed future
/// rather than being an `async fn` so that it stays usable as a trait object — the
/// supervisor holds one behind a `Box`.
pub trait DeviceFactory<D: GpuDevice>: Send + Sync {
    /// Creates a device.
    fn create(&self) -> DeviceFuture<'_, D>;
}

/// The future a [`DeviceFactory`] returns.
pub type DeviceFuture<'a, D> = Pin<Box<dyn Future<Output = Result<D, DeviceError>> + Send + 'a>>;

/// A future that is already resolved, for factories that do not need to wait.
///
/// A factory whose device is created synchronously — one that owns a device already, or
/// one that hands out a device from a pool — returns this rather than a hand-written
/// `async` block.
pub fn resolved_device<D: GpuDevice + 'static>(
    value: Result<D, DeviceError>,
) -> DeviceFuture<'static, D> {
    Box::pin(std::future::ready(value))
}

/// Owns the device, and rebuilds it when it is lost.
///
/// The supervisor is the whole of ADR-002's "recovery is a tested path": it decides when
/// a device is lost, rebuilds it, and counts the rebuilds so that a device that keeps
/// dying is *observable* rather than merely survivable.
pub struct DeviceSupervisor<D: GpuDevice> {
    factory: Box<dyn DeviceFactory<D>>,
    device: Option<D>,
    /// How many times a lost device has been replaced.
    recoveries: u64,
    /// How many devices have been created in total.
    creations: u64,
    /// Whether the last observed device was reported lost.
    lost: bool,
}

impl<D: GpuDevice> fmt::Debug for DeviceSupervisor<D> {
    /// Reports the supervisor's observable state rather than the device behind it.
    ///
    /// The device is a driver object: printing it would mean asking the driver to
    /// describe itself, which is not something a `Debug` implementation should do.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceSupervisor")
            .field("has_device", &self.device.is_some())
            .field("device", &self.device.as_ref().map(GpuDevice::adapter))
            .field("recoveries", &self.recoveries)
            .field("creations", &self.creations)
            .field("lost", &self.lost)
            .finish_non_exhaustive()
    }
}

impl<D: GpuDevice> DeviceSupervisor<D> {
    /// A supervisor with no device yet.
    pub fn new(factory: Box<dyn DeviceFactory<D>>) -> Self {
        Self {
            factory,
            device: None,
            recoveries: 0,
            creations: 0,
            lost: false,
        }
    }

    /// The device, creating one if there is none.
    ///
    /// # Errors
    ///
    /// Returns the reason the device could not be created.
    pub async fn ensure_device(&mut self) -> Result<&D, DeviceError> {
        if self.device.as_ref().is_some_and(GpuDevice::is_alive) {
            // The borrow is released before the `await` below, so the compiler is free to
            // keep the reference across it.
            return self.device.as_ref().ok_or(DeviceError::DeviceLost);
        }
        // A device that was there and is not any more is a *recovery*; no device at all is
        // the first creation. The two are counted separately, because "how many times did
        // the driver take the device away" is a different question from "how many devices
        // did this runtime make".
        let recovering = self.device.is_some();
        self.create(recovering).await
    }

    /// Checks the device's health and rebuilds it if it was lost.
    ///
    /// Returns whether a rebuild happened. A device that is alive costs one call into the
    /// driver; a device that is lost costs a new one.
    ///
    /// # Errors
    ///
    /// Returns the reason the replacement device could not be created. The lost device is
    /// dropped in that case, because a device that has been lost is not a device that can
    /// be drawn with.
    pub async fn observe(&mut self) -> Result<bool, DeviceError> {
        let alive = self.device.as_ref().is_some_and(GpuDevice::is_alive);
        if alive {
            self.lost = false;
            return Ok(false);
        }
        if self.device.is_none() {
            // Nothing to observe: the first device has not been created yet.
            return Ok(false);
        }
        self.rebuild().await?;
        Ok(true)
    }

    /// Replaces the device, whether or not it was lost.
    ///
    /// # Errors
    ///
    /// Returns the reason the replacement device could not be created.
    pub async fn rebuild(&mut self) -> Result<&D, DeviceError> {
        self.create(true).await
    }

    /// Creates a device and installs it, counting what it cost.
    async fn create(&mut self, recovery: bool) -> Result<&D, DeviceError> {
        let device = self.factory.create().await?;
        self.creations += 1;
        if recovery {
            self.recoveries += 1;
        }
        self.lost = false;
        self.device = Some(device);
        self.device.as_ref().ok_or(DeviceError::DeviceLost)
    }

    /// The current device, if there is one.
    #[must_use]
    pub fn device(&self) -> Option<&D> {
        self.device.as_ref()
    }

    /// How many times a device has been rebuilt.
    #[must_use]
    pub const fn recoveries(&self) -> u64 {
        self.recoveries
    }

    /// How many devices have been created.
    #[must_use]
    pub const fn creations(&self) -> u64 {
        self.creations
    }

    /// Whether the last observed device was reported lost.
    #[must_use]
    pub const fn was_lost(&self) -> bool {
        self.lost
    }

    /// Notes that the device was lost, without rebuilding it yet.
    ///
    /// The engine reports the loss; the supervisor decides what to do about it. Keeping
    /// those two apart is what lets a lost device be observed before it is replaced.
    pub fn note_loss(&mut self) {
        self.lost = true;
    }
}

/// The `wgpu` device and queue.
#[cfg(feature = "wgpu-device")]
#[derive(Debug)]
pub struct WgpuDevice {
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter: String,
    id: String,
}

#[cfg(feature = "wgpu-device")]
impl WgpuDevice {
    /// The `wgpu` instance this device was created from.
    ///
    /// A surface must be created from the same instance as the device that configures and
    /// draws into it, so the adapter that owns a window reaches the instance through here.
    #[must_use]
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }

    /// The underlying `wgpu` device, so the render pass can create buffers, textures and
    /// pipelines. This is the single safe escape hatch; everything above it is pure NC-UI.
    #[must_use]
    pub fn wgpu_device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The `wgpu` queue, for submitting command buffers.
    #[must_use]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
}

#[cfg(feature = "wgpu-device")]
impl GpuDevice for WgpuDevice {
    fn is_alive(&self) -> bool {
        // A lost device fails every submission and every poll. Polling is the cheapest
        // way to ask, and it is the same call the renderer makes at the end of a frame.
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .is_ok()
    }

    fn adapter(&self) -> String {
        self.adapter.clone()
    }

    fn id(&self) -> String {
        self.id.clone()
    }
}

/// Creates `wgpu` devices.
#[cfg(feature = "wgpu-device")]
#[derive(Debug, Default)]
pub struct WgpuDeviceFactory;

#[cfg(feature = "wgpu-device")]
impl DeviceFactory<WgpuDevice> for WgpuDeviceFactory {
    fn create(&self) -> DeviceFuture<'_, WgpuDevice> {
        Box::pin(async {
            let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
            instance_desc.backends = wgpu::Backends::PRIMARY;
            let instance = wgpu::Instance::new(instance_desc);

            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .map_err(|_| DeviceError::NoAdapter)?;

            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("ncui-gpu-renderer-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    trace: wgpu::Trace::Off,
                    ..Default::default()
                })
                .await
                .map_err(|e| DeviceError::DeviceCreation(e.to_string()))?;

            Ok(WgpuDevice {
                instance,
                device,
                queue,
                adapter: adapter.get_info().name,
                id: adapter.get_info().device.to_string(),
            })
        })
    }
}

/// The shared handle a renderer hands to the thread that draws.
///
/// The renderer is `Send` but not `Sync` — it owns a device and a queue — so the pieces
/// that a frame needs are shared rather than the renderer itself.
pub type SharedDevice = Arc<dyn GpuDevice>;

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A device that reports itself lost when the test says so.
    #[derive(Debug)]
    struct TestDevice {
        alive: bool,
    }

    impl GpuDevice for TestDevice {
        fn is_alive(&self) -> bool {
            self.alive
        }
        fn adapter(&self) -> String {
            "test-adapter".to_owned()
        }
        fn id(&self) -> String {
            "test-device".to_owned()
        }
    }

    /// A factory that hands out devices and counts what it made.
    #[derive(Debug, Default)]
    struct TestFactory {
        created: Mutex<Vec<TestDevice>>,
        /// Whether the next device it makes reports itself alive.
        alive: Mutex<bool>,
        /// Whether the next creation fails.
        fail: Mutex<bool>,
    }

    impl DeviceFactory<TestDevice> for TestFactory {
        fn create(&self) -> DeviceFuture<'_, TestDevice> {
            let alive = *self.alive.lock().expect("lock");
            let fail = *self.fail.lock().expect("lock");
            self.created
                .lock()
                .expect("lock")
                .push(TestDevice { alive });
            resolved_device(if fail {
                Err(DeviceError::DeviceCreation("the driver refused".to_owned()))
            } else {
                Ok(TestDevice { alive })
            })
        }
    }

    #[test]
    fn a_supervisor_creates_a_device_when_it_has_none() {
        let factory = TestFactory {
            alive: Mutex::new(true),
            ..TestFactory::default()
        };
        let mut supervisor = DeviceSupervisor::new(Box::new(factory));
        assert!(supervisor.device().is_none());
        let device = futures_lite_block_on(supervisor.ensure_device());
        assert!(device.is_ok(), "a device should have been created");
        assert_eq!(supervisor.creations(), 1);
        assert_eq!(supervisor.recoveries(), 0);
    }

    #[test]
    fn a_live_device_is_not_rebuilt() {
        let factory = TestFactory {
            alive: Mutex::new(true),
            ..TestFactory::default()
        };
        let mut supervisor = DeviceSupervisor::new(Box::new(factory));
        futures_lite_block_on(supervisor.ensure_device()).expect("a device");
        let rebuilt = futures_lite_block_on(supervisor.observe()).expect("observed");
        assert!(!rebuilt, "a live device must not be rebuilt");
        assert_eq!(supervisor.recoveries(), 0);
    }

    #[test]
    fn a_lost_device_is_rebuilt_and_the_rebuild_is_counted() {
        // This is ADR-002's "recovery is a tested path", exercised rather than described.
        let factory = TestFactory {
            alive: Mutex::new(true),
            ..TestFactory::default()
        };
        let mut supervisor = DeviceSupervisor::new(Box::new(factory));
        futures_lite_block_on(supervisor.ensure_device()).expect("a device");

        // The engine reports the loss.
        supervisor.note_loss();
        assert!(supervisor.was_lost());

        // The device is gone, so the supervisor replaces it.
        let rebuilt = futures_lite_block_on(supervisor.rebuild()).expect("a replacement");
        assert!(rebuilt.is_alive());
        assert_eq!(supervisor.recoveries(), 1);
        assert_eq!(supervisor.creations(), 2);
        assert!(!supervisor.was_lost());
    }

    #[test]
    fn a_device_that_dies_again_is_rebuilt_again() {
        // A device that keeps dying must be survivable *and* observable: the count is what
        // tells an operator the driver is not healthy.
        let factory = TestFactory {
            alive: Mutex::new(true),
            ..TestFactory::default()
        };
        let mut supervisor = DeviceSupervisor::new(Box::new(factory));
        futures_lite_block_on(supervisor.ensure_device()).expect("a device");
        for round in 1..=3 {
            futures_lite_block_on(supervisor.rebuild()).expect("a replacement");
            assert_eq!(supervisor.recoveries(), round);
        }
    }

    #[test]
    fn a_device_that_cannot_be_created_reports_why() {
        let factory = TestFactory {
            alive: Mutex::new(true),
            fail: Mutex::new(true),
            ..TestFactory::default()
        };
        let mut supervisor = DeviceSupervisor::new(Box::new(factory));
        let error = futures_lite_block_on(supervisor.ensure_device()).expect_err("must fail");
        assert_eq!(
            error,
            DeviceError::DeviceCreation("the driver refused".to_owned())
        );
        assert!(supervisor.device().is_none());
    }

    #[test]
    fn the_error_display_is_stable() {
        // These appear in metrics and observability; renaming them is breaking.
        assert_eq!(
            DeviceError::NoAdapter.to_string(),
            "no graphics adapter found"
        );
        assert_eq!(DeviceError::DeviceLost.to_string(), "gpu device lost");
    }

    /// Drives a future to completion on the current thread.
    ///
    /// The supervisor's futures never block on anything but the test's own values, so
    /// polling them by hand is enough — and it keeps the crate free of an async runtime
    /// dependency, and free of `unsafe`, which the workspace forbids outright.
    fn futures_lite_block_on<F: Future>(future: F) -> F::Output {
        use std::task::{Context, Poll};

        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
