//! Errors, written for the operator who has to act on them.
//!
//! Three absences look identical from a distance — no library, no board, no
//! LimeRFE support in the library that is there — and each has a different fix,
//! so each is its own error with the fix in the text.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// LimeSuite is not installed, or not findable.
    #[error("{0}")]
    LibMissing(String),

    /// No board matched what the configuration asked for.
    #[error("{0}")]
    NotFound(String),

    /// Another program holds it — LimeSuiteGUI, SDRangel, gqrx, a SoapySDR
    /// client, or another copy of this one.
    #[error(
        "{0} is held by another program (LimeSuiteGUI, SDRangel, gqrx, or a SoapySDR client) \
         — close it and try again"
    )]
    InUse(String),

    /// The library is present but predates LimeRFE support.
    #[error(
        "this LimeSuite build has no LimeRFE support (it arrived in 20.01) — upgrade it, or \
         connect the LimeRFE to its own USB port and choose that link instead"
    )]
    NoRfeSupport,

    /// The session was closed ahead of a reopen (see `LimeHandle::close`);
    /// its replacement owns the board now, or is about to.
    #[error("this LimeSDR session is closed while its replacement is opened — retry in a moment")]
    Closed,

    /// Any other API failure, with the call that failed named and LimeSuite's
    /// own words for it.
    #[error("LimeSuite {call} failed: {text}")]
    Api { call: &'static str, text: String },
}

impl Error {
    pub(crate) fn api(call: &'static str, text: String) -> Error {
        Error::Api { call, text }
    }
}
