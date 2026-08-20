# soapysdr — provenance

`vendor/soapysdr` is a copy of the **soapysdr** crate, Kevin Mehall's safe Rust
wrapper around the SoapySDR C API, reached through `[patch.crates-io]` in the
workspace manifest rather than as a workspace member.

| | |
|---|---|
| Upstream | <https://github.com/kevinmehall/rust-soapysdr> |
| Version | `0.5.1` (crates.io, the newest published release) |
| Author | Kevin Mehall `<km@kevinmehall.net>` and contributors |
| Licence | BSL-1.0 OR Apache-2.0 |

Both `LICENSE` (Apache-2.0) and `LICENSE-BSL` are upstream's own files, copied
unchanged. Both are inbound-compatible with this workspace's GPL-3.0-or-later,
so the built binary is GPL as before and upstream's code stays under upstream's
terms.

## What is patched

**One added method**, `Device::setting_info`, in `src/device.rs`. Nothing else
is changed: no upstream line is edited, removed or reordered.

The manifest drops the two `[[example]]` targets and the three
`dev-dependencies` that existed only to build them, because the `examples/`
directory is not copied. That is a packaging trim, not a code change.

## Why the patch exists

SoapySDR drivers carry their own settings — a HackRF's `bias_tx`, an RTL-SDR's
`direct_samp`, an RSP's `rfnotch_ctrl` — and the C API describes them through
`SoapySDRDevice_getSettingInfo`: the key, the type, the default, and the
permitted options. Upstream wraps `readSetting` and `writeSetting` but not that
enumeration call, and the raw `*mut SoapySDRDevice` it needs is private to
`device.rs` (`DeviceInner.ptr`, with no accessor).

The consequence is not cosmetic. Without it a host can only set a setting whose
name it has hard-coded — which is exactly the device-specific branching that
choosing SoapySDR is supposed to avoid. sdroxide's SoapySDR settings panel is
built on the enumeration, so every driver's own controls appear without
sdroxide knowing anything about that driver.

`setting_info` is the same shape as upstream's own `stream_args_info` one field
over: it introduces no new unsafety and no new FFI helper, only a second caller
of the existing `arg_info_result`. That is deliberate — it is meant to be
trivially reviewable, and to be dropped the moment upstream carries it.

## How to remove this

When upstream publishes the method, delete `vendor/soapysdr`, drop the
`[patch.crates-io]` block and the `exclude` entry from the workspace manifest,
and raise the `soapysdr` version in `crates/sdroxide-radio/Cargo.toml`. Nothing
else in the workspace refers to the fork.
