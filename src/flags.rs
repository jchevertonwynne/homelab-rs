//! Arguments in the shape Go's `flag` package reads them.
//!
//! The Deployments pass `-addr :8092 -otel-endpoint host:4317`, and they keep
//! doing so across a rewrite: a Rust binary that wanted `--addr` instead would
//! crash-loop on the first rollout. So this accepts what `flag` accepts — one
//! dash or two, the value as the next argument or after `=` — and fails the
//! way it fails: unknown flag, usage on stderr, exit status 2.

use std::collections::HashMap;
use std::fmt;

struct Def {
    name: &'static str,
    default: &'static str,
    usage: &'static str,
}

/// The flags a program takes. Every one is a string with a default; that is
/// all any app here has ever needed.
#[derive(Default)]
pub struct Flags {
    defs: Vec<Def>,
}

/// The values after parsing, defaults included.
#[derive(Debug)]
pub struct Parsed {
    values: HashMap<&'static str, String>,
}

impl Parsed {
    /// The value of a flag that was defined. Asking for one that was not is a
    /// bug in the program, not in its arguments, so it panics.
    pub fn get(&self, name: &str) -> &str {
        self.values
            .get(name)
            .unwrap_or_else(|| panic!("flag -{name} was never defined"))
    }
}

#[derive(Debug, PartialEq)]
pub enum Error {
    Help,
    Undefined(String),
    NeedsArgument(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Help => write!(f, "flag: help requested"),
            Error::Undefined(name) => write!(f, "flag provided but not defined: -{name}"),
            Error::NeedsArgument(name) => write!(f, "flag needs an argument: -{name}"),
        }
    }
}

impl Flags {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn string(
        mut self,
        name: &'static str,
        default: &'static str,
        usage: &'static str,
    ) -> Self {
        self.defs.push(Def {
            name,
            default,
            usage,
        });
        self
    }

    /// Parses the process's own arguments, exiting as `flag.Parse` does on a
    /// bad one: 0 for `-h`, 2 for anything else.
    pub fn parse(self) -> Parsed {
        let mut args = std::env::args();
        let program = args.next().unwrap_or_default();
        match self.parse_from(args) {
            Ok(parsed) => parsed,
            Err(err) => {
                if err != Error::Help {
                    eprintln!("{err}");
                }
                eprint!("{}", self.usage(&program));
                std::process::exit(if err == Error::Help { 0 } else { 2 });
            }
        }
    }

    /// Parsing stops at the first argument that is not a flag, or after `--`,
    /// as it does in Go. Nothing here takes positional arguments, so whatever
    /// follows is ignored rather than rejected, which is also what Go does.
    pub fn parse_from(&self, args: impl IntoIterator<Item = String>) -> Result<Parsed, Error> {
        let mut values: HashMap<&'static str, String> = self
            .defs
            .iter()
            .map(|d| (d.name, d.default.to_string()))
            .collect();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--" || !arg.starts_with('-') || arg == "-" {
                break;
            }
            let body = arg.strip_prefix("--").unwrap_or(&arg[1..]);
            let (name, inline) = match body.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (body, None),
            };
            if name == "h" || name == "help" {
                return Err(Error::Help);
            }
            let Some(def) = self.defs.iter().find(|d| d.name == name) else {
                return Err(Error::Undefined(name.to_string()));
            };
            let value = match inline {
                Some(value) => value,
                None => args
                    .next()
                    .ok_or_else(|| Error::NeedsArgument(name.to_string()))?,
            };
            values.insert(def.name, value);
        }
        Ok(Parsed { values })
    }

    /// `flag.PrintDefaults`, near enough that the help reads the same.
    fn usage(&self, program: &str) -> String {
        let mut out = format!("Usage of {program}:\n");
        for d in &self.defs {
            out.push_str(&format!("  -{} string\n    \t{}", d.name, d.usage));
            if !d.default.is_empty() {
                out.push_str(&format!(" (default {:?})", d.default));
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> Flags {
        Flags::new()
            .string("addr", ":8080", "listen address")
            .string("otel-endpoint", "", "collector")
    }

    fn parse(args: &[&str]) -> Result<Parsed, Error> {
        flags().parse_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn takes_every_spelling_go_takes() {
        for args in [
            &["-addr", ":9"][..],
            &["--addr", ":9"],
            &["-addr=:9"],
            &["--addr=:9"],
        ] {
            assert_eq!(parse(args).unwrap().get("addr"), ":9", "{args:?}");
        }
    }

    #[test]
    fn defaults_stand_until_overridden() {
        let parsed = parse(&["-otel-endpoint", "alloy:4317"]).unwrap();
        assert_eq!(parsed.get("addr"), ":8080");
        assert_eq!(parsed.get("otel-endpoint"), "alloy:4317");
    }

    #[test]
    fn rejects_what_go_rejects() {
        assert_eq!(
            parse(&["-port", "1"]).unwrap_err(),
            Error::Undefined("port".into())
        );
        assert_eq!(
            parse(&["-addr"]).unwrap_err(),
            Error::NeedsArgument("addr".into())
        );
        assert_eq!(parse(&["-h"]).unwrap_err(), Error::Help);
    }

    #[test]
    fn stops_at_the_first_positional_argument() {
        let parsed = parse(&["extra", "-addr", ":9"]).unwrap();
        assert_eq!(parsed.get("addr"), ":8080");
    }
}
