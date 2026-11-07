# Local compatibility patch

Upstream: `longbridge/nsh`, tag `v0.4.0`, crates.io `nsh-platform-gpui 0.4.0`.
Published archive SHA-256, verified locally:
`1e3828245c26e734567b3e6058398c0e4d5809063636460f990a0eaf8a0f74c3`.
License: Apache-2.0; upstream copyright and license preserved in `LICENSE-APACHE` (copied from the same upstream tag because the published crate omitted that file).

Only product-source modification: `src/chrome.rs` private `native_items` receives `&App` and uses `KeyBinding::load` for dynamically generated menu shortcuts. GPUI Fast 0.1.13 changed `KeyBinding::new` to require `&'static str`; the unmodified NSH 0.4.0 source fails compilation. Invalid dynamic shortcuts now log an error rather than panic. No string leaks. Recursive and root callers pass the existing App reference.

Scope: isolated candidate workspace only. The default editor does not depend on this patched crate. Revisit/remove the patch when a compatible upstream release is evaluated. Keep the spike lockfile: rolldown 1.2.13 also requires compatible 1.2.13 helper crates rather than semver-selected 1.3.0 helpers.
