//! Command-line flags as osewa's command line has always taken them, so
//! scripts written for any osewa keep working: `-name value`, `--name
//! value`, `-name=value`, bare boolean flags, and flags ending at `--` or
//! the first non-flag argument. Also duration strings ("300ms", "2h45m").

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone)]
enum Kind {
    Str,
    Bool,
}

#[derive(Debug, Clone)]
struct Def {
    kind: Kind,
    default: String,
    usage: String,
}

/// A set of flag definitions.
#[derive(Debug, Clone, Default)]
pub struct FlagSet {
    name: String,
    defs: BTreeMap<String, Def>,
}

/// The parsed values and the arguments left after the flags.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    values: BTreeMap<String, String>,
    set: Vec<String>,
    pub args: Vec<String>,
}

impl FlagSet {
    pub fn new(name: &str) -> FlagSet {
        FlagSet { name: name.into(), defs: BTreeMap::new() }
    }

    /// Defines a flag that takes a value (string, int or duration).
    pub fn value(&mut self, name: &str, default: &str, usage: &str) -> &mut Self {
        self.defs.insert(name.into(), Def { kind: Kind::Str, default: default.into(), usage: usage.into() });
        self
    }

    /// Defines a boolean flag: `-name`, `-name=true` or `-name=false`.
    pub fn bool(&mut self, name: &str, default: bool, usage: &str) -> &mut Self {
        self.defs.insert(name.into(), Def { kind: Kind::Bool, default: default.to_string(), usage: usage.into() });
        self
    }

    /// The usage text for the set, in osewa's usual layout.
    pub fn usage(&self) -> String {
        let mut s = format!("Usage of {}:\n", self.name);
        for (name, d) in &self.defs {
            match d.kind {
                Kind::Bool => s.push_str(&format!("  -{name}\n")),
                Kind::Str => s.push_str(&format!("  -{name} value\n")),
            }
            s.push_str(&format!("    \t{}", d.usage));
            if !d.default.is_empty() && d.default != "false" && d.default != "0" {
                s.push_str(&format!(" (default {:?})", d.default));
            }
            s.push('\n');
        }
        s
    }

    pub fn parse<S: AsRef<str>>(&self, args: &[S]) -> Result<Parsed, Error> {
        let mut p = Parsed::default();
        for (k, d) in &self.defs {
            p.values.insert(k.clone(), d.default.clone());
        }
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_ref();
            if a.len() < 2 || !a.starts_with('-') {
                break;
            }
            if a == "--" {
                i += 1;
                break;
            }
            let body = a.strip_prefix("--").unwrap_or(&a[1..]);
            if body.is_empty() || body.starts_with('-') || body.starts_with('=') {
                return Err(Error(format!("bad flag syntax: {a}")));
            }
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            if name == "h" || name == "help" {
                return Err(Error("help requested".into()));
            }
            let def = self.defs.get(name).ok_or_else(|| Error(format!("flag provided but not defined: -{name}")))?;
            let value = match def.kind {
                Kind::Bool => {
                    let v = inline.unwrap_or_else(|| "true".into());
                    parse_bool(&v).ok_or_else(|| Error(format!("invalid boolean value {v:?} for -{name}")))?.to_string()
                }
                Kind::Str => match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i).map(|s| s.as_ref().to_string()).ok_or_else(|| Error(format!("flag needs an argument: -{name}")))?
                    }
                },
            };
            p.values.insert(name.into(), value);
            p.set.push(name.into());
            i += 1;
        }
        p.args = args[i..].iter().map(|s| s.as_ref().to_string()).collect();
        Ok(p)
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

impl Parsed {
    pub fn str(&self, name: &str) -> String {
        self.values.get(name).cloned().unwrap_or_default()
    }

    pub fn bool(&self, name: &str) -> bool {
        self.values.get(name).map(|v| v == "true").unwrap_or(false)
    }

    /// Whether the flag was given on the command line.
    pub fn is_set(&self, name: &str) -> bool {
        self.set.iter().any(|n| n == name)
    }

    pub fn int(&self, name: &str) -> Result<i64, Error> {
        let v = self.str(name);
        parse_int(&v).ok_or_else(|| Error(format!("invalid value {v:?} for flag -{name}: parse error")))
    }

    pub fn duration(&self, name: &str) -> Result<Duration, Error> {
        let v = self.str(name);
        parse_duration(&v).map_err(|e| Error(format!("invalid value {v:?} for flag -{name}: {e}")))
    }
}

/// Parses an integer flag value: decimal, 0x, 0o,
/// 0b, or a leading 0 for octal, with an optional sign and underscores.
pub fn parse_int(s: &str) -> Option<i64> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let clean: String = body.chars().filter(|&c| c != '_').collect();
    let (radix, digits) = if let Some(d) = clean.strip_prefix("0x").or(clean.strip_prefix("0X")) {
        (16, d.to_string())
    } else if let Some(d) = clean.strip_prefix("0o").or(clean.strip_prefix("0O")) {
        (8, d.to_string())
    } else if let Some(d) = clean.strip_prefix("0b").or(clean.strip_prefix("0B")) {
        (2, d.to_string())
    } else if clean.len() > 1 && clean.starts_with('0') {
        (8, clean[1..].to_string())
    } else {
        (10, clean)
    };
    if digits.is_empty() {
        return None;
    }
    let v = i64::from_str_radix(&digits, radix).ok()?;
    Some(if neg { -v } else { v })
}

/// Parses a duration string such as "300ms", "1.5h" or "2h45m" (units ns,
/// us/µs, ms, s, m, h). "0" needs no unit and negative durations are
/// rejected here because
/// no osewa flag takes one.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    if s.is_empty() {
        return Err("invalid duration \"\"".into());
    }
    let bad = || format!("invalid duration {s:?}");
    let mut rest = s.strip_prefix('+').unwrap_or(s);
    if rest.starts_with('-') {
        return Err(bad());
    }
    let mut total = 0f64;
    while !rest.is_empty() {
        let num_end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let num = &rest[..num_end];
        if num.is_empty() || num == "." {
            return Err(bad());
        }
        let n: f64 = num.parse().map_err(|_| bad())?;
        rest = &rest[num_end..];
        let unit_end = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        rest = &rest[unit_end..];
        let ns = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            "" => return Err(format!("missing unit in duration {s:?}")),
            _ => return Err(format!("unknown unit {unit:?} in duration {s:?}")),
        };
        total += n * ns;
    }
    Ok(Duration::from_nanos(total.round() as u64))
}

/// Formats a duration the way parse_duration reads it: "1h2m3.5s", "300ms".
pub fn format_duration(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns == 0 {
        return "0s".into();
    }
    if ns < 1_000 {
        return format!("{ns}ns");
    }
    if ns < 1_000_000 {
        return format!("{}µs", trim_float(ns as f64 / 1e3));
    }
    if ns < 1_000_000_000 {
        return format!("{}ms", trim_float(ns as f64 / 1e6));
    }
    let mut out = String::new();
    let secs = ns / 1_000_000_000;
    let frac = ns % 1_000_000_000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    let sf = s as f64 + frac as f64 / 1e9;
    out.push_str(&format!("{}s", trim_float(sf)));
    out
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.9}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> FlagSet {
        let mut fs = FlagSet::new("holder");
        fs.value("run", "", "run dir").value("cols", "80", "columns").value("idle-timeout", "24h", "idle").bool("quic", false, "q");
        fs
    }

    #[test]
    fn flag_syntax() {
        let p = set().parse(&["--run", "/r", "-cols=100", "-quic", "--", "sh", "-c", "x"]).unwrap();
        assert_eq!(p.str("run"), "/r");
        assert_eq!(p.int("cols").unwrap(), 100);
        assert!(p.bool("quic") && p.is_set("quic") && !p.is_set("idle-timeout"));
        assert_eq!(p.duration("idle-timeout").unwrap(), Duration::from_secs(86400));
        assert_eq!(p.args, ["sh", "-c", "x"]);
        // Flags stop at the first non-flag.
        let p = set().parse(&["--cols", "5", "bash", "--run", "x"]).unwrap();
        assert_eq!(p.args, ["bash", "--run", "x"]);
        assert_eq!(p.str("run"), "");
        assert!(set().parse(&["--nope"]).is_err());
        assert!(set().parse(&["--run"]).is_err());
        assert!(set().parse(&["---run", "x"]).is_err());
        assert!(set().parse(&["-quic=maybe"]).is_err());
        assert!(!set().parse(&["-quic=false"]).unwrap().bool("quic"));
        assert!(set().parse(&["-h"]).is_err());
        assert_eq!(set().parse(&["-"]).unwrap().args, ["-"]);
        assert!(set().usage().contains("-cols value"));
    }

    #[test]
    fn ints() {
        assert_eq!(parse_int("42"), Some(42));
        assert_eq!(parse_int("-0x10"), Some(-16));
        assert_eq!(parse_int("010"), Some(8));
        assert_eq!(parse_int("1_000"), Some(1000));
        assert_eq!(parse_int("x"), None);
        assert_eq!(parse_int(""), None);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("300us").unwrap(), Duration::from_micros(300));
        assert_eq!(parse_duration("2µs").unwrap(), Duration::from_micros(2));
        assert_eq!(parse_duration("10ns").unwrap(), Duration::from_nanos(10));
        for bad in ["", "5", "1x", "-1s", ".s", "h"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert_eq!(format_duration(Duration::from_secs(86400)), "24h0m0s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_duration(Duration::from_millis(200)), "200ms");
        assert_eq!(format_duration(Duration::from_micros(3)), "3µs");
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m30s");
    }
}
