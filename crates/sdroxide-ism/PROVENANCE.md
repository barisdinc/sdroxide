# rtl_433 — provenance

`vendor/rtl_433` is a git submodule holding **rtl_433**, Benjamin Larsson's and
Christian Zuckschwerdt's decoder collection for ISM-band devices. It is used
**unmodified**: no upstream line is edited, removed or reordered, and nothing is
copied out of it into sdroxide's own sources.

| | |
|---|---|
| Upstream | <https://github.com/merbanan/rtl_433> |
| Pinned at | `8fa6364c5c7e14665fe3d80d0553883ec14a4116` (2026-07-28) |
| Author | Benjamin Larsson, Christian W. Zuckschwerdt and contributors |
| Licence | GPL-2.0-or-later |

## Licence effect

GPL-2.0-**or-later** is inbound-compatible with this workspace's
GPL-3.0-or-later: the "or later" clause lets rtl_433's code be conveyed under
GPL-3, which is the licence the combined binary is distributed under. Upstream's
code stays under upstream's terms.

This is a real change to `sdroxide-ism`, which previously carried no GPL
dependency at all and said so. With the `rtl433` feature on — it is on by
default — that crate now links GPL-2.0-or-later code, and the resulting binary
carries those obligations alongside the AGPL-3.0 ones it already has via
`sdroxide-deepcw`. Building with `--no-default-features` (or any feature set
without `rtl433`) leaves rtl_433 out entirely, and `sdroxide-ism` is then pure
Rust with no C dependency.

The native decoders in `crates/sdroxide-ism/src/proto/` are unaffected by any of
this: each was written from its protocol's published specification, cites that
specification in its own header, and remains clean-room. None of them came from
rtl_433, before or after this submodule was added.

## Why a master commit rather than a release tag

The pinned commit is on `master`, not on the `25.12` release tag. That is
deliberate.

Upstream factored its whole demodulation flow into a small public API —
`push_sdr_flow()`, `reset_sdr_flow()` and `flush_sdr_flow()` in
`include/r_flow.h` — in commit `2d029fb` on 2026-07-18, after 25.12 shipped. That
API is exactly what an embedder feeding its own samples needs, and it is the one
sdroxide uses.

In 25.12 the same work is only reachable through `sdr_callback()`, which is
`static` and lives in `src/rtl_433.c` — the one file upstream's own `r_433`
library target excludes, because it holds `main()`. Building against the release
would therefore mean reimplementing a hundred-odd lines of upstream's internal
buffer handling in our shim and re-checking it against upstream on every bump.
Using the published entry point is both less code and less drift.

A submodule pins a commit, not a branch, so this is exactly as reproducible as a
tag would be.

## How it is built

`crates/sdroxide-ism/build.rs` runs upstream's own CMake and builds its `r_433`
static library target — everything except `main()`. The source list is upstream's,
so a decoder added upstream is picked up by moving the submodule and nothing else.

Three CMake options are forced off, all for the same reason — sdroxide feeds
rtl_433 samples from its own device backends and must not link a second SDR
stack:

- `ENABLE_RTLSDR=OFF` — **not** merely a default: upstream defaults it **ON** and
  makes it a fatal error when librtlsdr is missing, so without this the build
  would depend on a library sdroxide deliberately does not use.
- `ENABLE_SOAPYSDR=OFF`
- `ENABLE_OPENSSL=OFF`

Two more are set for the build to work at all:

- `BUILD_TESTING=OFF` — upstream's `include(CTest)` turns it on by default.
- `CMAKE_POLICY_VERSION_MINIMUM=3.5` — upstream still declares
  `cmake_minimum_required(VERSION 2.6...3.10)`, which CMake 4 refuses outright.

The only C written by sdroxide is `crates/sdroxide-ism/src/rtl433/shim.c`, which
is ours, GPL-3.0-or-later like the rest of the workspace, and calls upstream
through its public headers.

## Updating the submodule

When moving the pin, re-check these three, all of which mirror upstream source
rather than a documented interface:

1. **`crates/sdroxide-ism/src/rtl433/flex.rs`** — the keyword and modulation
   whitelists, and the fatal-path list, mirror `src/devices/flex.c`. rtl_433
   reports a bad decoder spec by calling `exit()`, so this validator is the only
   thing standing between a pasted spec and a dead process. If upstream adds a
   keyword, a spec using it is refused until the list is updated; if upstream
   adds a *fatal check*, the validator must learn it or the process can die.
2. **`crates/sdroxide-ism/src/rtl433/mod.rs`** — `COVERED_NATIVES` lists the
   native protocols rtl_433 covers better. Upstream has no Z-Wave and no
   Homematic decoder, so those two must never appear there.
3. **`shim.c`'s `sdrx_sync_demod_params()`** — copies the per-callback demod
   parameters that `sdr_handler()` sets in `src/rtl_433.c`. `raw_handler` in
   particular is dereferenced unconditionally by `push_sdr_flow()` and is NULL on
   a config that has never been through upstream's own SDR path.
4. **`demod->auto_level`** — set in `sdrx_rtl433_create()`, and load-bearing.
   It is `-Y autolevel` on the command line and `r_create_cfg()` leaves it at 0,
   which pins the minimum detection level at `min_level` (≈ −12 dBFS). That
   suits an RTL-SDR, which sets its own gain and delivers samples near full
   scale; it does not suit a decimated window from a receiver with no gain
   control, where a sensor burst is 35–65 dB down. With it unset **nothing
   decodes at all** and there is no error to see. `tests/rtl433_weak_signal.rs`
   is the guard: it fails if this line goes away.

If a whole band stops decoding after a bump, set `SDROXIDE_RTL433_VERBOSITY=4`
and look for rtl_433's own "Auto Level: estimated noise level is …" lines; they
say whether the detector has found the floor. They arrive through `tracing`.
