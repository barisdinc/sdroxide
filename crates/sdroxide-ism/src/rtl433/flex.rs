//! User-authored decoders: rtl_433 "flex" specs, read from a config file.
//!
//! This is the extension point an operator has without writing Rust. A flex spec
//! describes a device's modulation and framing as key/value text, and rtl_433
//! turns it into a working decoder — so the community's published specs can be
//! pasted in unchanged and simply work.
//!
//! # Why this module is strict
//!
//! `flex_create_device()` in rtl_433 reports every problem by printing to stderr
//! and calling `exit()`. On the command line that is reasonable. Linked into a
//! long-running receiver it is not: one mistyped keyword in a pasted spec would
//! take the whole application down, mid-QSO, with nothing on screen to say why.
//!
//! So nothing reaches rtl_433 that has not passed [`validate`] first, which
//! mirrors every fatal path in `flex_create_device()` and its helpers. Where a
//! rule is hard to reproduce exactly, this module is deliberately *stricter*
//! than rtl_433: refusing a spec that would have worked is a message in a panel,
//! while accepting one that would not is a dead process.
//!
//! The rules mirror `vendor/rtl_433/src/devices/flex.c`. Re-read it when the
//! submodule moves.

/// Keywords `flex_create_device()` accepts. Anything else is fatal there.
///
/// Mirrors the `strcasecmp` chain in `flex_create_device()`.
const KEYWORDS: &[&str] = &[
    "n",
    "name",
    "m",
    "modulation",
    "s",
    "short",
    "l",
    "long",
    "y",
    "sync",
    "g",
    "gap",
    "r",
    "reset",
    "t",
    "tolerance",
    "prio",
    "priority",
    "bits",
    "bits>",
    "bits<",
    "rows",
    "rows>",
    "rows<",
    "repeats",
    "repeats>",
    "repeats<",
    "invert",
    "reflect",
    "match",
    "preamble",
    "countonly",
    "unique",
    "decode_uart",
    "decode_dm",
    "decode_mc",
    "symbol_zero",
    "symbol_one",
    "symbol_sync",
    "get",
];

/// Modulations `parse_modulation()` accepts.
const MODULATIONS: &[&str] = &[
    "OOK_MC_ZEROBIT",
    "OOK_PCM",
    "OOK_RZ",
    "OOK_PPM",
    "OOK_PWM",
    "OOK_DMC",
    "OOK_PIWM_RAW",
    "OOK_PIWM_DC",
    "OOK_MC_OSV1",
    "FSK_PCM",
    "FSK_PWM",
    "FSK_MC_ZEROBIT",
];

/// Modulations that carry one symbol width, so `long` is not required.
const NO_LONG_NEEDED: &[&str] = &["OOK_MC_ZEROBIT", "FSK_MC_ZEROBIT"];

/// Modulations whose pulse slicing needs an explicit tolerance.
const TOLERANCE_NEEDED: &[&str] = &["OOK_DMC", "OOK_PIWM_RAW", "OOK_PIWM_DC"];

/// Keys parsed with `parse_float()`, which exits on anything not wholly numeric.
const FLOAT_KEYS: &[&str] =
    &["s", "short", "l", "long", "y", "sync", "g", "gap", "r", "reset", "t", "tolerance"];

/// Keys parsed with `parse_atoiv()`, which exits unless the value starts with a
/// number — but accepts an empty value, treating it as "present, use the default".
const INT_KEYS: &[&str] = &[
    "prio",
    "priority",
    "bits",
    "bits>",
    "bits<",
    "rows",
    "rows>",
    "rows<",
    "repeats",
    "repeats>",
    "repeats<",
    "invert",
    "reflect",
    "countonly",
    "unique",
    "decode_dm",
    "decode_mc",
];

/// Keys whose value goes through `parse_bits()`: one bit row, at most 1024 bits.
const BITS_KEYS: &[&str] = &["match", "preamble"];

/// Keys whose value goes through `parse_symbol()`: one bit row, at most 27 bits.
const SYMBOL_KEYS: &[&str] = &["symbol_zero", "symbol_one", "symbol_sync"];

/// UART modes `parse_uart_mode()` accepts.
const UART_MODES: &[&str] = &["8n1", "8n2", "8o1"];

/// `GETTER_SLOTS` in flex.c.
const GETTER_SLOTS: usize = 12;

/// One decoder spec read from the config file, with where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// The text handed to rtl_433, brace block flattened out.
    pub spec: String,
    /// 1-based line the block started on, for error messages.
    pub line: usize,
}

/// A spec that was refused, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub line: usize,
    pub message: String,
}

/// Read a flex config file.
///
/// Understands the two shapes rtl_433's own conf files use, so a spec copied
/// from its `conf/` directory or from a forum post works as-is:
///
/// ```text
/// decoder {
///     name=ELRO-AB440R,
///     modulation=OOK_PWM,
///     ...
/// }
///
/// decoder n=doorbell,m=OOK_PWM,s=400,l=800,r=7000
/// ```
///
/// A bare `n=...` line with no `decoder` keyword is accepted too — that is the
/// form the `-X` command line takes, and it is what someone experimenting is
/// most likely to paste.
///
/// Returns the specs that parsed and the problems for those that did not.
/// A bad block never stops the ones after it: one unusable spec should cost its
/// own decoder, not the whole file.
pub fn parse_conf(text: &str) -> (Vec<Spec>, Vec<Problem>) {
    let mut specs = Vec::new();
    let mut problems = Vec::new();

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line_no = i + 1;
        let line = strip_comment(lines[i]).trim();
        if line.is_empty() {
            i += 1;
            continue;
        }

        // `decoder` keyword is optional; strip it if present.
        let rest = match line.strip_prefix("decoder") {
            Some(r) if r.is_empty() || r.starts_with([' ', '\t', '{']) => r.trim(),
            _ => line,
        };

        if let Some(open) = rest.strip_prefix('{') {
            // Brace block: gather until the closing brace.
            let mut body = String::from(open);
            let mut closed = body.contains('}');
            if closed {
                body = body[..body.find('}').unwrap()].to_string();
            }
            while !closed && i + 1 < lines.len() {
                i += 1;
                let l = strip_comment(lines[i]);
                if let Some(end) = l.find('}') {
                    body.push(' ');
                    body.push_str(&l[..end]);
                    closed = true;
                } else {
                    body.push(' ');
                    body.push_str(l);
                }
            }
            if !closed {
                problems.push(Problem {
                    line: line_no,
                    message: "decoder block is never closed — a '}' is missing".into(),
                });
                i += 1;
                continue;
            }
            push_spec(&mut specs, &mut problems, flatten(&body), line_no);
        } else if !rest.is_empty() {
            push_spec(&mut specs, &mut problems, flatten(rest), line_no);
        }
        i += 1;
    }

    (specs, problems)
}

fn push_spec(specs: &mut Vec<Spec>, problems: &mut Vec<Problem>, spec: String, line: usize) {
    if spec.is_empty() {
        return;
    }
    match validate(&spec) {
        Ok(()) => specs.push(Spec { spec, line }),
        Err(message) => problems.push(Problem { line, message }),
    }
}

/// Drop a `#` comment, respecting nothing else — same as rtl_433's conf parser.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Collapse a possibly multi-line block into one comma-separated spec.
///
/// rtl_433 tolerates the whitespace itself (`remove_ws` on keys, `trim_ws` on
/// values), but a flattened spec is far easier to show back in an error message.
/// A trailing comma from the last line of a block is dropped.
fn flatten(body: &str) -> String {
    let joined: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.trim().trim_end_matches(',').trim().to_string()
}

/// Split a spec the way `getkwargs()` does: on commas, then the first `=`.
fn kwargs(spec: &str) -> Vec<(String, Option<&str>)> {
    spec.split(',')
        .map(|pair| match pair.find('=') {
            Some(i) => (remove_ws(&pair[..i]), Some(pair[i + 1..].trim())),
            None => (remove_ws(pair), None),
        })
        .collect()
}

/// `remove_ws()` in flex.c strips whitespace anywhere in the key, not just at
/// the ends — which is what lets a key survive a line break inside a block.
fn remove_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Check a spec against every fatal path in `flex_create_device()`.
///
/// `Ok` means rtl_433 will parse it without calling `exit()`. It does not mean
/// the decoder will hear anything — only the operator's radio can settle that.
pub fn validate(spec: &str) -> Result<(), String> {
    let trimmed = spec.trim();

    // flex.c: `!spec || !*spec || *spec == '?' || !strncasecmp(spec, "help", strlen(spec))`
    // prints usage and exits. Note the length used is the *spec's*, so any
    // prefix of "help" triggers it too.
    if trimmed.is_empty() {
        return Err("the spec is empty".into());
    }
    if trimmed.starts_with('?') {
        return Err("a spec starting with '?' asks rtl_433 for help text".into());
    }
    if !trimmed.is_empty()
        && trimmed.len() <= 4
        && "help".starts_with(&trimmed.to_ascii_lowercase())
    {
        return Err(format!(
            "\"{trimmed}\" asks rtl_433 for help text rather than describing a decoder"
        ));
    }

    let pairs = kwargs(trimmed);

    let mut name: Option<&str> = None;
    let mut modulation: Option<String> = None;
    let mut floats: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    let mut has_symbol_zero = false;
    let mut has_symbol_one = false;
    let mut getters = 0usize;

    for (key, val) in &pairs {
        if key.is_empty() {
            // flex.c skips an empty key outright.
            continue;
        }
        let lower = key.to_ascii_lowercase();
        if !KEYWORDS.contains(&lower.as_str()) {
            return Err(format!(
                "unknown keyword \"{key}\" — rtl_433 would refuse this spec and stop the program, so sdroxide is not passing it on"
            ));
        }

        let v = val.unwrap_or("").trim();

        if FLOAT_KEYS.contains(&lower.as_str()) {
            // parse_float() exits on missing, empty, non-numeric, or trailing junk.
            if v.is_empty() {
                return Err(format!("\"{key}\" needs a number"));
            }
            let parsed: f64 =
                v.parse().map_err(|_| format!("\"{key}\" is not a number: \"{v}\""))?;
            if !parsed.is_finite() {
                return Err(format!("\"{key}\" is not a finite number: \"{v}\""));
            }
            floats.insert(canonical(&lower), parsed);
            continue;
        }

        if INT_KEYS.contains(&lower.as_str()) {
            // parse_atoiv() takes an empty value as "use the default"; a
            // non-empty one must start with digits.
            if !v.is_empty() && !starts_with_int(v) {
                return Err(format!("\"{key}\" is not a whole number: \"{v}\""));
            }
            continue;
        }

        match lower.as_str() {
            "n" | "name" => {
                if v.is_empty() {
                    return Err("\"name\" is empty".into());
                }
                name = Some(v);
            }
            "m" | "modulation" => {
                let m = v.to_ascii_uppercase();
                if !MODULATIONS.contains(&m.as_str()) {
                    return Err(format!(
                        "unknown modulation \"{v}\" — rtl_433 accepts {}",
                        MODULATIONS.join(", ")
                    ));
                }
                modulation = Some(m);
            }
            "decode_uart" => {
                if !UART_MODES.contains(&v.to_ascii_lowercase().as_str()) {
                    return Err(format!(
                        "unknown uart mode \"{v}\" — rtl_433 accepts {}",
                        UART_MODES.join(", ")
                    ));
                }
            }
            k if BITS_KEYS.contains(&k) => {
                check_bit_pattern(v, 1024, key)?;
            }
            k if SYMBOL_KEYS.contains(&k) => {
                check_bit_pattern(v, 27, key)?;
                match k {
                    "symbol_zero" => has_symbol_zero = true,
                    "symbol_one" => has_symbol_one = true,
                    _ => {}
                }
            }
            "get" => {
                getters += 1;
                if getters > GETTER_SLOTS {
                    return Err(format!(
                        "more than {GETTER_SLOTS} \"get\" fields — rtl_433 allows no more"
                    ));
                }
                check_getter(v)?;
            }
            _ => {}
        }
    }

    // The sanity checks at the end of flex_create_device(), in its order.
    if name.is_none() {
        return Err("no \"name\" — every decoder needs one".into());
    }
    let Some(modulation) = modulation else {
        return Err("no \"modulation\" — see rtl_433's flex documentation for the list".into());
    };
    if floats.get("short").copied().unwrap_or(0.0) == 0.0 {
        return Err("no \"short\" width".into());
    }
    if !NO_LONG_NEEDED.contains(&modulation.as_str())
        && floats.get("long").copied().unwrap_or(0.0) == 0.0
    {
        return Err(format!("no \"long\" width, which {modulation} needs"));
    }
    if floats.get("reset").copied().unwrap_or(0.0) == 0.0 {
        return Err("no \"reset\" limit".into());
    }
    if TOLERANCE_NEEDED.contains(&modulation.as_str())
        && floats.get("tolerance").copied().unwrap_or(0.0) == 0.0
    {
        return Err(format!("no \"tolerance\", which {modulation} needs"));
    }
    if has_symbol_zero != has_symbol_one {
        return Err("\"symbol_zero\" and \"symbol_one\" have to be given together".into());
    }

    Ok(())
}

/// Map an abbreviation onto the long key, so the checks above read as one name.
fn canonical(key: &str) -> &'static str {
    match key {
        "s" | "short" => "short",
        "l" | "long" => "long",
        "y" | "sync" => "sync",
        "g" | "gap" => "gap",
        "r" | "reset" => "reset",
        "t" | "tolerance" => "tolerance",
        _ => "other",
    }
}

fn starts_with_int(v: &str) -> bool {
    let v = v.trim_start();
    let v = v.strip_prefix(['-', '+']).unwrap_or(v);
    v.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Vet a value that rtl_433 would hand to `bitbuffer_parse()`.
///
/// `bitbuffer_parse` itself never fails — unknown characters silently repeat the
/// previous nibble — but its callers do: `parse_bits` and `parse_symbol` exit
/// unless the result is exactly one row within a length limit. A second `{...}`
/// or a `/` starts another row, so both are refused here.
///
/// Anything that is not a plain `{count}hex` or bare hex pattern is refused as
/// well. rtl_433 would accept more, but silently, as different bits than the
/// author meant.
fn check_bit_pattern(v: &str, max_bits: usize, key: &str) -> Result<(), String> {
    let v = v.trim();
    if v.is_empty() {
        return Err(format!("\"{key}\" is empty"));
    }
    if v.contains('/') {
        return Err(format!(
            "\"{key}\" has a '/' — that is a second bit row, which rtl_433 refuses here"
        ));
    }

    let (declared, rest) = match v.strip_prefix('{') {
        Some(after) => {
            let Some(end) = after.find('}') else {
                return Err(format!("\"{key}\" has a '{{' with no '}}'"));
            };
            let count: usize = after[..end]
                .trim()
                .parse()
                .map_err(|_| format!("\"{key}\" has a bit count that is not a number"))?;
            (Some(count), after[end + 1..].trim())
        }
        None => (None, v),
    };

    if rest.contains('{') {
        return Err(format!(
            "\"{key}\" has more than one {{...}} bit count, which rtl_433 reads as a second row"
        ));
    }

    let digits = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")).unwrap_or(rest);
    let digits: String = digits.chars().filter(|c| !c.is_whitespace()).collect();
    if !digits.is_empty() && !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "\"{key}\" is not a hex bit pattern: \"{rest}\" — write it as {{bits}}hex, e.g. {{24}}0xa9878c"
        ));
    }

    let bits = declared.unwrap_or(digits.len() * 4);
    if bits == 0 {
        return Err(format!("\"{key}\" has no bits"));
    }
    if bits > max_bits {
        return Err(format!("\"{key}\" has {bits} bits; rtl_433 allows at most {max_bits}"));
    }
    Ok(())
}

/// Vet a `get=` field. `parse_getter()` exits when no name is left over.
///
/// Shape is `@offset:{bits}mask:name` with an optional `[a:a b:b]` value map and
/// an optional `%format`, all parts optional except the name.
fn check_getter(v: &str) -> Result<(), String> {
    let v = v.trim();
    if v.is_empty() {
        return Err("\"get\" is empty".into());
    }
    // The value map is scanned separately by rtl_433 and cannot contain a name.
    let head = match v.find('[') {
        Some(i) => &v[..i],
        None => v,
    };

    let mut has_name = false;
    for part in head.split(':') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.starts_with('@') || part.starts_with('%') {
            continue;
        }
        if part.starts_with('{') || part.starts_with(|c: char| c.is_ascii_digit()) {
            check_bit_pattern(part, 1024, "get")?;
            continue;
        }
        has_name = true;
    }
    if !has_name {
        return Err("\"get\" has no field name — write it as @0:{8}:temperature".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Specs copied verbatim from rtl_433's own conf/ directory. If any of these
    /// stop validating, this module has drifted from what it mirrors.
    #[test]
    fn accepts_real_community_specs() {
        let cases = [
            // conf/elro_ab440r.conf
            "name=ELRO-AB440R, modulation=OOK_PWM, short=330, long=970, gap=1200, reset=9000, bits=25, symbol_zero={2}8, symbol_one={2}c, get=@0:{5}:channel, get=@5:{4}:button:[8:A 4:B 2:C 1:D], get=@10:{2}:toggle:[2:ON 1:OFF], unique",
            // the -X form from rtl_433's documentation
            "n=doorbell,m=OOK_PWM,s=400,l=800,r=7000,g=1000,match={24}0xa9878c",
            // manchester needs no long width
            "n=mc,m=OOK_MC_ZEROBIT,s=100,r=2000,bits>=20",
            "n=fsk,m=FSK_PCM,s=100,l=100,r=2000,preamble={16}0x2dd4",
        ];
        for c in cases {
            assert!(validate(c).is_ok(), "should accept: {c}\n{:?}", validate(c));
        }
    }

    #[test]
    fn refuses_what_would_kill_the_process() {
        // Each of these hits an exit() inside flex_create_device().
        let cases: &[(&str, &str)] = &[
            ("", "empty"),
            ("?", "help"),
            ("help", "help"),
            ("hel", "help"),
            ("n=x,m=OOK_PWM,s=1,l=2,r=3,bogus=4", "unknown keyword"),
            ("n=x,m=NOT_A_MOD,s=1,l=2,r=3", "unknown modulation"),
            ("m=OOK_PWM,s=1,l=2,r=3", "name"),
            ("n=x,s=1,l=2,r=3", "modulation"),
            ("n=x,m=OOK_PWM,l=2,r=3", "short"),
            ("n=x,m=OOK_PWM,s=1,r=3", "long"),
            ("n=x,m=OOK_PWM,s=1,l=2", "reset"),
            ("n=x,m=OOK_DMC,s=1,l=2,r=3", "tolerance"),
            ("n=x,m=OOK_PWM,s=one,l=2,r=3", "not a number"),
            ("n=x,m=OOK_PWM,s=1,l=2,r=3,symbol_zero={2}8", "together"),
            ("n=x,m=OOK_PWM,s=1,l=2,r=3,decode_uart=9n9", "uart"),
        ];
        for (spec, want) in cases {
            let err = validate(spec).expect_err(&format!("should refuse: {spec}"));
            assert!(
                err.to_lowercase().contains(want),
                "refused {spec:?} with {err:?}, expected mention of {want:?}"
            );
        }
    }

    #[test]
    fn refuses_multi_row_bit_patterns() {
        // parse_bits() exits unless exactly one row comes back.
        for spec in [
            "n=x,m=OOK_PWM,s=1,l=2,r=3,match={8}aa/{8}bb",
            "n=x,m=OOK_PWM,s=1,l=2,r=3,match={8}aa{8}bb",
        ] {
            assert!(validate(spec).is_err(), "should refuse: {spec}");
        }
    }

    #[test]
    fn refuses_oversized_symbols() {
        // parse_symbol() caps a symbol at 27 bits.
        assert!(validate("n=x,m=OOK_PWM,s=1,l=2,r=3,symbol_zero={28}0,symbol_one={2}1").is_err());
        assert!(validate("n=x,m=OOK_PWM,s=1,l=2,r=3,symbol_zero={2}8,symbol_one={2}c").is_ok());
    }

    #[test]
    fn refuses_too_many_getters() {
        let mut spec = String::from("n=x,m=OOK_PWM,s=1,l=2,r=3");
        for i in 0..=GETTER_SLOTS {
            spec.push_str(&format!(",get=@0:{{2}}:f{i}"));
        }
        assert!(validate(&spec).is_err());
    }

    #[test]
    fn parses_a_brace_block() {
        let text = "\
# a comment
decoder {
    name=ELRO-AB440R,
    modulation=OOK_PWM,
    short=330,
    long=970,
    gap=1200,
    reset=9000,
    bits=25,
    unique
}
";
        let (specs, problems) = parse_conf(text);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(specs.len(), 1);
        assert!(specs[0].spec.contains("name=ELRO-AB440R"));
        // Flattened onto one line, with the block's trailing comma gone.
        assert!(!specs[0].spec.contains('\n'));
        assert!(specs[0].spec.ends_with("unique"));
    }

    #[test]
    fn parses_single_line_forms() {
        let text = "\
decoder n=a,m=OOK_PWM,s=1,l=2,r=3
n=b,m=OOK_PWM,s=1,l=2,r=3
";
        let (specs, problems) = parse_conf(text);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn one_bad_block_does_not_lose_the_others() {
        let text = "\
decoder n=good1,m=OOK_PWM,s=1,l=2,r=3
decoder n=bad,m=OOK_PWM,s=1,l=2,r=3,nonsense=1
decoder n=good2,m=OOK_PWM,s=1,l=2,r=3
";
        let (specs, problems) = parse_conf(text);
        assert_eq!(specs.len(), 2, "{specs:?}");
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].line, 2);
        assert!(problems[0].message.contains("nonsense"));
    }

    #[test]
    fn reports_an_unclosed_block() {
        let (specs, problems) = parse_conf("decoder {\n  name=x,\n");
        assert!(specs.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].message.contains("never closed"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let (specs, problems) = parse_conf("# nothing here\n\n   \n# nor here\n");
        assert!(specs.is_empty());
        assert!(problems.is_empty());
    }

    #[test]
    fn a_comment_after_a_spec_is_stripped() {
        let (specs, problems) = parse_conf("decoder n=a,m=OOK_PWM,s=1,l=2,r=3  # my doorbell\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(specs.len(), 1);
        assert!(!specs[0].spec.contains("doorbell"));
    }
}
