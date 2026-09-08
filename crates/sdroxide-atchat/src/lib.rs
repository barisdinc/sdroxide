//! AtCHAT NET — an OFDM multi-station keyboard/file mode.
//!
//! A 2.7 kHz-wide COFDM waveform carrying a small NET protocol: dynamic master
//! election, a shared roster, common + directed chat, and block-CRC-ARQ file
//! and image transfer. Ported from barisdinc/AtChat.
//!
//! `modem` is the real OFDM modulator/demodulator (8 kHz, 256-point FFT,
//! Schmidl-Cox sync, differential BPSK/QPSK). There is no FEC; the upper
//! layer's ARQ does the correcting. `netproto` holds the shared constants,
//! frame types and the line-JSON wire used by the virtual channel (which is
//! `channel_server.py`-compatible).
//!
//! `protocol` (the `Station` state machine), `channel` (the virtual TCP
//! channel + link abstraction) and `engine` (the `DigiEngine` seam) land in
//! later commits.

pub mod modem;
pub mod netproto;
