Vendored from `vqsort-rs` 0.3.0, upstream commit
`fbdb693e2b7ce865fd315d72d4172555e4d9e255`:
https://github.com/lincot/vqsort-rs

Crate archive SHA-256:
`a7fe26506ab511743d7d406e0efd9bb9155b14dbbb036a2f4ce194198a4fd6ca`.

The only code change is in `build.rs`: use `pkg-config`'s discovered Highway
include paths when compiling the C++ shim. Upstream 0.3.0 probes for Highway
but does not pass the returned include paths to `cc`, so a clean build fails
when Highway is installed outside the compiler's default include directories.
Unused upstream dev/optional dependencies and package metadata were omitted
from this local wrapper manifest; the Rust and C++ library sources are
unchanged.

The wrapper is Apache-2.0 OR BSD-3-Clause. It links to Google Highway, which
is Apache-2.0. Both licenses are permissive and compatible with this
repository's MIT license; see `LICENSE` and `LICENSE-BSD3`.
