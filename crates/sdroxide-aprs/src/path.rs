//! Digipeater paths, and the one rule about them that matters.
//!
//! APRS shares one channel per region with everybody in radio range of
//! everybody, and the only thing keeping it usable is how far each station
//! asks to be repeated. Under the "New-N" paradigm a path is written
//! `WIDEn-N`: `n` is how many hops the sender wants and `N` counts down as
//! each digipeater passes it on. `WIDE1-1,WIDE2-1` — one local fill-in hop
//! then one wide one — is the right answer almost everywhere, and
//! `WIDE2-2` for a mobile that wants a little more reach.
//!
//! What is not the right answer is a long path. Every extra hop multiplies the
//! number of transmissions the whole network makes on one channel, and a
//! station running `WIDE3-3` on a busy channel is not getting further out; it
//! is stopping other people being heard at all.

/// Split a path as an operator writes it: comma-separated, spaces ignored.
#[must_use]
pub fn parse_path(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_ascii_uppercase())
        .filter(|p| !p.is_empty())
        // AX.25 allows eight digipeaters; APRS practice never needs three.
        .take(8)
        .collect()
}

/// Total hops a path asks for — the sum of every `WIDEn-N`'s `N`, plus one for
/// each plain callsign, which is one specific digipeater and therefore one hop.
#[must_use]
pub fn hop_count(path: &[String]) -> u32 {
    path.iter().map(|p| hops_of(p)).sum()
}

fn hops_of(p: &str) -> u32 {
    match p.split_once('-') {
        Some((_, n)) => n.parse().unwrap_or(1),
        None => 1,
    }
}

/// Why a path should not be used, if there is a reason.
///
/// Advice rather than enforcement: local practice varies, a path that is
/// wasteful in a European city is reasonable in the Australian outback, and
/// nothing here refuses to transmit. The setup dialog shows the sentence.
#[must_use]
pub fn path_advice(path: &[String]) -> Option<&'static str> {
    if path.is_empty() {
        return None;
    }
    // The pre-New-N aliases. A digipeater built this decade ignores them, so a
    // station using one is transmitting into a path nothing will repeat.
    for p in path {
        let base = p.split('-').next().unwrap_or(p);
        if matches!(base, "RELAY" | "TRACE" | "WIDE" | "ECHO" | "GATE") && !p.contains('-') {
            return Some(
                "RELAY, TRACE and a bare WIDE are the pre-2004 aliases. Modern digipeaters \
                 ignore them, so nothing will repeat this. Use WIDE1-1,WIDE2-1.",
            );
        }
    }
    let hops = hop_count(path);
    if hops > 3 {
        return Some(
            "More than three hops. Every hop multiplies the transmissions the whole network \
             makes on one shared channel; past three it stops other stations being heard \
             without getting you any further out. WIDE1-1,WIDE2-1 reaches almost anywhere.",
        );
    }
    // A fill-in digipeater answers WIDE1-1 and nothing else, so a first hop
    // that is not WIDE1-1 skips the digipeaters closest to you.
    if path.len() > 1 && path[0].starts_with("WIDE") && path[0] != "WIDE1-1" {
        return Some(
            "A first hop other than WIDE1-1 skips the fill-in digipeaters, which are the \
             ones nearest you and the reason a low-power station gets out at all.",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usual_path_parses_and_is_not_complained_about() {
        let p = parse_path("wide1-1, wide2-1");
        assert_eq!(p, vec!["WIDE1-1", "WIDE2-1"]);
        assert_eq!(hop_count(&p), 2);
        assert_eq!(path_advice(&p), None);
    }

    #[test]
    fn a_deprecated_alias_is_called_out() {
        assert!(path_advice(&parse_path("RELAY,WIDE2-2")).is_some());
    }

    #[test]
    fn too_many_hops_is_called_out() {
        assert!(path_advice(&parse_path("WIDE2-2,WIDE3-3")).is_some());
        assert_eq!(hop_count(&parse_path("WIDE2-2,WIDE3-3")), 5);
    }

    /// A specific digipeater by callsign is one hop, not zero.
    #[test]
    fn a_named_digipeater_counts_as_a_hop() {
        assert_eq!(hop_count(&parse_path("OE3XLR,WIDE2-1")), 2);
        assert_eq!(path_advice(&parse_path("OE3XLR,WIDE2-1")), None);
    }
}
