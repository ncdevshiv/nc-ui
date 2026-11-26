- **Windows remembered-window recovery now survives empty display enumeration.** When the
  engine's monitor list is empty but the platform can still identify its primary display, the
  host reports that primary display instead of leaving remembered geometry unclamped. The live
  application test that stores an off-screen window place now verifies that the restored window
  lands on a real display while keeping its requested size.
