//! AX.25 addresses: a callsign, an SSID, and four bits that mean different
//! things depending on where the address sits.
//!
//! Vendored from rax25 `src/lib.rs`; see `vendor/rax25/PROVENANCE.md`. Two
//! changes from upstream:
//!
//! * the `regex` validation is hand-rolled, so a link layer does not pull a
//!   regex engine in for one pattern;
//! * the SSID is kept as a field rather than re-parsed out of the string on
//!   every serialize, which removes an `unwrap` whose safety depended on a
//!   validation that happened somewhere else entirely.

use crate::Ax25Error;

type Result<T> = std::result::Result<T, Ax25Error>;

/// An AX.25 address: callsign, SSID, and the bits that ride with them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Addr {
    pub(crate) t: String,
    /// 0..=15. Held rather than re-parsed out of `t` on every serialize.
    pub(crate) ssid: u8,
    pub(crate) rbit_ext: bool,

    // Most significant bit of the last byte.
    //
    // * On src/dst this is command/response per LA PA 6.1.2.
    // * On repeater it's "has been repeated".
    pub(crate) highbit: bool,

    // The least significant bit on the last byte. Used to indicate more
    // repeaters are provided.
    pub(crate) lowbit: bool,
    pub(crate) rbit_dama: bool,
}

impl Addr {
    /// Create a new Addr from string. The extra bits are all clear.
    ///
    /// Accepts `CALL` or `CALL-SSID`, three to six alphanumerics and an SSID of
    /// 0..=15 — upstream's regex `^[A-Z0-9]{3,6}(?:-(?:[0-9]|1[0-5]))?$`,
    /// written out so this crate need not link a regex engine for one pattern.
    pub fn new(s: &str) -> Result<Self> {
        let s = s.to_uppercase();
        let bad = || Ax25Error::Callsign(s.clone());
        let (base, ssid) = match s.split_once('-') {
            Some((b, tail)) => {
                // Reject "-01" and "-1x" as well as out-of-range: the SSID is
                // one or two digits and no more.
                if tail.is_empty()
                    || tail.len() > 2
                    || !tail.bytes().all(|c| c.is_ascii_digit())
                    || (tail.len() == 2 && tail.starts_with('0'))
                {
                    return Err(bad());
                }
                let n: u8 = tail.parse().map_err(|_| bad())?;
                if n > 15 {
                    return Err(bad());
                }
                (b, n)
            }
            None => (s.as_str(), 0),
        };
        if !(3..=6).contains(&base.len()) || !base.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err(bad());
        }
        Ok(Self {
            t: s.clone(),
            ssid,
            rbit_ext: false,
            highbit: false,
            lowbit: false,
            rbit_dama: false,
        })
    }

    /// The SSID, 0..=15.
    #[must_use]
    pub fn ssid(&self) -> u8 {
        self.ssid
    }

    /// Set lowbit.
    #[must_use]
    pub fn set_highbit(mut self, v: bool) -> Self {
        self.highbit = v;
        self
    }

    /// Set lowbit.
    #[must_use]
    pub fn set_lowbit(mut self, v: bool) -> Self {
        self.lowbit = v;
        self
    }

    /// Create a new Addr from string and the extra bits.
    pub fn new_bits(
        s: &str,
        lowbit: bool,
        highbit: bool,
        rbit_ext: bool,
        rbit_dama: bool,
    ) -> Result<Self> {
        let mut a = Self::new(s)?;
        a.lowbit = lowbit;
        a.highbit = highbit;
        a.rbit_ext = rbit_ext;
        a.rbit_dama = rbit_dama;
        Ok(a)
    }

    /// The high bit of the SSID byte.
    ///
    /// It means two different things depending on where the address sits. In
    /// the source and destination it is half of the command/response pair. In
    /// a *digipeater* address it is the "has been repeated" flag — which is
    /// what an APRS monitor prints as the asterisk in `WIDE1-1*`, and the only
    /// way to tell which hop of a path a frame actually came through.
    #[must_use]
    pub fn highbit(&self) -> bool {
        self.highbit
    }

    /// Get just the callsign & SSID as a string, not the extra bits.
    #[must_use]
    pub fn call(&self) -> &str {
        &self.t
    }

    /// Parse the callsign and the extra bits from the packet format.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 7 {
            return Err(Ax25Error::AddrLength(bytes.len()));
        }
        let call = {
            let call = bytes
                .iter()
                .take(6)
                .map(|&c| (c >> 1) as char)
                .collect::<String>()
                .trim_end()
                .to_string();
            let ssid = (bytes[6] >> 1) & 15;
            if ssid > 0 { call + "-" + &ssid.to_string() } else { call }
        };
        Self::new_bits(
            &call,
            bytes[6] & 1 != 0,
            bytes[6] & 0x80 != 0,
            bytes[6] & 0b0100_0000 == 0,
            bytes[6] & 0b0010_0000 == 0,
        )
    }

    /// Serialize the callsign and SSID, plus explicit bits.
    #[must_use]
    pub fn serialize(
        &self,
        lowbit: bool,
        highbit: bool,
        rbit_ext: bool,
        rbit_dama: bool,
    ) -> Vec<u8> {
        // TODO: confirm format.
        let mut ret = vec![b' ' << 1; 7];
        for (i, ch) in self.t.chars().take(6).enumerate() {
            if ch == '-' {
                break;
            }
            ret[i] = (ch as u8) << 1;
        }
        ret[6] = (self.ssid << 1)
            | (if rbit_ext { 0 } else { 0b0100_0000 })
            | (if rbit_dama { 0 } else { 0b0010_0000 })
            | u8::from(lowbit)
            | (if highbit { 0x80 } else { 0 });
        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 7-byte encoding, against bytes taken from a real frame rather than
    /// from this code: callsign shifted left one bit, padded with spaces, SSID
    /// in bits 1..4 of the last byte.
    #[test]
    fn a_callsign_round_trips_through_the_wire_form() {
        let a = Addr::new("OE3JJS").unwrap();
        let bytes = a.serialize(true, false, false, false);
        assert_eq!(
            &bytes[..6],
            &[b'O' << 1, b'E' << 1, b'3' << 1, b'J' << 1, b'J' << 1, b'S' << 1]
        );
        assert_eq!(Addr::parse(&bytes).unwrap().call(), "OE3JJS");
    }

    /// A short callsign is space-padded to six characters, not truncated or
    /// left ragged — a receiver trims the padding, so getting this wrong
    /// produces a callsign nobody answers to.
    #[test]
    fn a_short_callsign_is_space_padded() {
        let bytes = Addr::new("W1AW").unwrap().serialize(true, false, false, false);
        assert_eq!(&bytes[4..6], &[b' ' << 1, b' ' << 1]);
        assert_eq!(Addr::parse(&bytes).unwrap().call(), "W1AW");
    }

    #[test]
    fn an_ssid_survives_the_round_trip() {
        for ssid in 0..=15u8 {
            let call = if ssid == 0 { "OE3JJS".to_string() } else { format!("OE3JJS-{ssid}") };
            let a = Addr::new(&call).unwrap();
            assert_eq!(a.ssid(), ssid);
            let bytes = a.serialize(true, false, false, false);
            assert_eq!(Addr::parse(&bytes).unwrap().call(), call);
        }
    }

    /// The hand-rolled validation must accept and reject exactly what
    /// upstream's regex did — `^[A-Z0-9]{3,6}(?:-(?:[0-9]|1[0-5]))?$`.
    #[test]
    fn validation_matches_the_regex_it_replaced() {
        for good in ["W1AW", "OE3JJS", "OE3JJS-0", "OE3JJS-9", "OE3JJS-15", "N0CALL-1", "abc"] {
            assert!(Addr::new(good).is_ok(), "{good} should be a valid callsign");
        }
        for bad in [
            "",
            "AB",
            "TOOLONGCALL",
            "OE3JJS-16",
            "OE3JJS-",
            "OE3JJS-01",
            "OE3JJS-1x",
            "OE3-JJS",
            "OE3JJS-99",
            "OE/JJS",
            "OE3JJS-1-2",
        ] {
            assert!(Addr::new(bad).is_err(), "{bad} should be rejected");
        }
    }

    /// Lower case is accepted and normalised — an operator typing their own
    /// call in lower case should not be told it is invalid.
    #[test]
    fn callsigns_are_upper_cased() {
        assert_eq!(Addr::new("oe3jjs-7").unwrap().call(), "OE3JJS-7");
    }
}
