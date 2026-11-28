- **GPU rendering is compiled in by default and can no longer silently skip.** `wgpu-device`
  and `text-shaping` are now default features of `ncui-gpu-renderer`, so the workspace test
  leg executes the real render pipeline everywhere. When a machine has no hardware GPU the
  device creation falls back to wgpu's software adapter (WARP on Windows, llvmpipe on Linux)
  instead of refusing — the same shaders execute, which is what makes a GPU-less CI runner a
  real test rather than a skip. `NCUI_GPU_FORCE_SOFTWARE=1` pins the software path so the
  fallback itself is provable on hardware machines; the render suite passes through it on
  Windows. `make gate` gained the GPU-enabled leg: the runtime island tests (13 tests over
  real HTTP) execute instead of compiling into zero tests.
