- **The Windows release pipeline now passes end to end.** After provisioning
  `cargo-cyclonedx`, the pipeline generated a 103-component SBOM and two clean release builds in
  the same target directory produced the identical SHA-256
  `ee3be54581946f5a7b72c55243b9a6643cf76e078bedc5fd6db49742511cb32f`. The cross-directory
  `OUT_DIR` limitation remains documented; no signing key is configured on this machine, so the
  detached-signature step is still unexercised.
