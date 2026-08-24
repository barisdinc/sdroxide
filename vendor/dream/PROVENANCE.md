# dream — provenance

`vendor/dream` is a source subset of the **Dream** AM/DRM receiver, the
long-running open-source Digital Radio Mondiale implementation begun at
Technische Universität Darmstadt. It is built into a static library by
`crates/sdroxide-drm`, which wraps it behind a small C API.

| | |
|---|---|
| Upstream | <https://sourceforge.net/projects/drm/> |
| Version | `2.2` (released 2019-05-08, the newest release) |
| Tarball | `dream_2.2.orig.tar.gz` |
| SHA-256 | `f7211ee3c19b42116b6d1f999d45007c1a9e62fee92906aa37d56eb00219ef56` |
| Author | Volker Fischer, Julian Cable, Stéphane Fillod, David Flamand and contributors |
| Licence | GPL-2.0-or-later |

`COPYING`, `AUTHORS` and `README` are upstream's own files, copied unchanged.
GPL-2.0-**or-later** matters: the built binary also links mfsk-core
(GPL-3.0-or-later) and the DeepCW model (AGPL-3.0-only), and only the
"or later" makes that combination possible at all.

Upstream is a Subversion repository with no git remote, which is why this is a
copied tree rather than a submodule — the same situation as `vendor/soapysdr`.
The tarball above is the complete original; what is here is the part that is
built.

## What this links against

`crates/sdroxide-drm` also builds **faad2** (`vendor/faad2`, a git submodule
pinned to `2.11.2`, GPL-2.0-or-later) with `DRM_SUPPORT`, and links it directly.
Dream normally `dlopen`s a `libfaad_drm` at runtime instead, which most systems
do not have — leaving a receiver that acquires the signal, reads the service
label, and plays silence. Building it in is what makes the feature work out of
the box.

## What was removed

Nothing that is compiled. The Qt user interface (`src/GUI-QT`, `src/main-Qt`,
`src/util-QT`), the Android sound backends (`src/android`), the empty
`src/macx`, and the sound-card and console code under `src/linux` and
`src/windows` that the shim replaces — `alsa*`, `jack*`, `ConsoleIO*`,
`shmsoundin*`, `pa_shm_ringbuffer*`, `Sound.*`. `Pacer.cpp` and
`platform_util.*` are kept because they are built. The top-level `debian`,
`windows`, `macx`, `linux`, `DreamTests`, `libs` and the qmake project are not
copied.

## What is patched

Ten files, and no upstream line is edited except as described. Five of them are
there because Dream was written for one receiver on one thread and sdroxide
runs one per radio, on a thread each, fed by whatever a shortwave broadcast
happens to contain.

**`src/sound/sound.h`** — one added branch. Under `USE_SDROXIDE_SOUND` the
`CSoundIn`/`CSoundOut` typedefs resolve to the ring-buffer shims in
`crates/sdroxide-drm/include/sdrx_sound.h` instead of a sound card, and the two
fallback conditions (the null interface, and the Windows mmsystem branch) are
guarded so they no longer win. sdroxide feeds the decoder samples from its own
receive chain, so there is no sound card in this picture at all.

**`src/sourcedecoders/aac_codec.cpp`** — two changes.

1. *A bug fix.* `AacCodec::Decode` copied a fixed `AUD_DEC_TRANSFROM_LENGTH`
   (960) samples per channel out of faad2's internal buffer, ignoring
   `NeAACDecFrameInfo::samples` — and fell off the end of the function without
   returning a value, which is undefined behaviour in its own right. faad2 2.10
   and later report a *successful* decode with `samples == 0` for the first SBR
   frame of a stream, so the copy runs off the end of the decoder's buffer and
   segfaults within seconds of acquiring any real broadcast. It now takes the
   length faad2 reports, bounded by the destination, zero-fills any remainder,
   and returns a proper `EDecError`. This is why upstream Dream 2.2 crashes
   against a current faad2, and the fix is not sdroxide-specific.
2. Under `SDROXIDE_NO_AAC_ENCODER`, the constructor no longer `dlopen`s a FAAC
   *encoder*. sdroxide never transmits DRM, and the probe only produced a
   failure message on stderr.

**`src/sourcedecoders/opus_codec.cpp`** — under `SDROXIDE_QUIET`, the two
messages announcing whether libopus was found are not printed. A library has no
business writing to stderr in a GUI application; sdroxide logs through
`tracing`.

**`src/DrmReceiver.cpp`** — one removed `cerr` line that printed every
enumerated input device name while selecting one.

**`src/sourcedecoders/AudioCodec.{h,cpp}`** — the codec list is per thread, and
building it is serialised.

Upstream shares one list of codec objects across every `CAudioSourceDecoder` in
the program, reference-counted with a plain `int` and no lock. Two receivers
starting at the same moment double-free the vector's storage — reproducible in
seconds under AddressSanitizer, and a SIGSEGV about once in forty tries in an
ordinary build. That is only the first thing to go wrong: `GetDecoder` hands
both receivers the *same* `AacCodec`, which owns one faad2 handle, so one
receiver's `DecClose` frees the decoder state the other is still decoding
through. `thread_local` fixes both, because the invariant already holds — a
Dream receiver may only be touched from the thread that built it, so its audio
decoder and its codec live and die together on that thread. Construction and
destruction still take a mutex, because a codec constructor can `dlopen` a
library and fill a table of *global* function pointers from it. Nothing on the
decode path takes that lock.

**`src/matlib/MatlibStdToolbox.cpp`** — the FFT plan mutex is a function-local
static instead of a raw pointer lazily assigned in `CFftPlans`' constructor.
Two threads building their first plan both saw it null, both allocated, and one
then locked a mutex the other was not using. C++11 guarantees exactly one
thread runs a function-local static's initializer, which is the deferred
construction the original comment ("static initialization of CMutex not working
on Mac OS X") was reaching for.

**`src/datadecoding/DABMOT.cpp`** — the MOT reassembler is bounded, and one
out-of-bounds write is fixed.

Every offset it computes is *segment number × segment size*, and both come
straight off the air: the segment number is a 15-bit field in the segmentation
header and the segment size is whatever the last packet happened to carry.
Nothing upstream checks the product. Two things follow. A corrupt header asks
for an allocation of hundreds of megabytes — and since `CVector::Enlarge` takes
an `int`, past `INT_MAX` the size wraps negative, the vector *shrinks*, and the
copy that follows writes far outside it. Separately, `copylast()` grew the
vector by the last segment's own length while writing at the offset the last
segment's *number* implies, which only coincides when every earlier segment
arrived and was exactly full — one short segment and the write runs past the
end. Both reassemblers (`CReassembler`, `CBitReassembler`) now bound the offset
and size the vector from the offset. The identical bug in
`src/util/Reassemble.cpp`'s `CReassemblerN` is left alone: it is only used by
PFT, which is only reached over an RSCI network input this build never enables.

This matters because it is broadcast data reaching a parser with no bounds
checks: a station carrying a slideshow or an EPG runs thousands of lines that a
plain audio-only station never touches, which is exactly the shape of "it works
on most stations and kills the radio on one".

**`src/datadecoding/DataDecoder.cpp`** — under `SDROXIDE_NO_DATA_FILES`, a
decoded EPG object is no longer written out, and the `fwrite` of an empty
object's `front()` is guarded.

This was the only place in the whole receiver that touched the filesystem, and
it took the filename from the broadcast. With the data directory set to `"."`
— which is what the shim asks for — a station carrying an EPG would create
`./EPG/…` in whatever the host's working directory happened to be, under a
name the transmission chose. sdroxide surfaces no EPG, so there was nothing to
write in the first place.

**`src/datadecoding/journaline/NML.cpp`** — `#include <zlib.h>` made
conditional on `HAVE_LIBZ`, which is how its sibling `DABMOT.cpp` already
guards the same header, and the one function that uses zlib
(`Inflate`, for *compressed* Journaline objects) returns failure when it is not
built in. The caller already handles that — "could not uncompress NML body" —
and uncompressed objects are unaffected.

This is the only thing in the tree that wanted a system library nothing else
here needs. It built anywhere `zlib.h` happened to be installed, which included
every developer machine and the GitHub runner images, and failed on a clean
distribution container; the symbols then resolved only because some unrelated
dependency had pulled `-lz` onto the link line. sdroxide surfaces no Journaline
at all, so the whole question is moot for this build — but it is exactly the
kind of accidental dependency that shows up first in somebody else's release.
