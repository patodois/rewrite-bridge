//! A ready-made `check-conformance` subcommand for a provider crate's command line.
//!
//! ```no_run
//! use rewrite_bridge::{AssetInfo, BoxError, Model, Provider};
//!
//! struct MyGame;
//! impl Provider for MyGame {
//!     fn game(&self) -> &str { "My Game" }
//!     fn list(&self, _filter: &str) -> Result<Vec<AssetInfo>, BoxError> { Ok(vec![]) }
//!     fn load(&self, _id: &str) -> Result<Model, BoxError> { Ok(Model::default()) }
//! }
//!
//! fn main() {
//!     let mut args = std::env::args().skip(1);
//!     if args.next().as_deref() == Some("check-conformance") {
//!         std::process::exit(rewrite_bridge::conformance::cli::run(&MyGame, args));
//!     }
//!     // ... the crate's other subcommands
//! }
//! ```

use std::io::Write;

use super::provider::{check_provider_with, ProviderOptions};
use super::report::{tally, Report, Severity};
use crate::{AssetKind, Provider};

/// The help text.
pub const USAGE: &str = "\
check-conformance [options]
  Loads assets through the provider one at a time and checks them against the bridge's conventions.

  --id ID          check this asset (repeatable); default: every listed asset
  --filter TEXT    check assets whose id or name contains TEXT
  --kind KIND      only assets of this kind: vehicle, character, weapon, prop, level, other (repeatable)
  --limit N        stop after N assets (N at least 1)
  --compare-load   also require load(id) to equal load_animated(id).model
  --allow CODE     ignore findings with this code (repeatable)
  --verbose, -v    list info findings too (default: warnings and errors)
  --strict         exit 1 on warnings as well as errors
  --help, -h       this text

Exit status: 0 if nothing failed, 1 if an asset has an error (or a warning with --strict), 2 on a
usage error.";

fn parse_kind(s: &str) -> Option<AssetKind> {
    Some(match s.to_ascii_lowercase().as_str() {
        "vehicle" => AssetKind::Vehicle,
        "character" => AssetKind::Character,
        "weapon" => AssetKind::Weapon,
        "prop" => AssetKind::Prop,
        "level" => AssetKind::Level,
        "other" => AssetKind::Other,
        _ => return None,
    })
}

/// Runs the checks the arguments ask for (see [`USAGE`]) and prints to stdout; returns the exit
/// status for `std::process::exit`.
pub fn run(provider: &dyn Provider, args: impl IntoIterator<Item = String>) -> i32 {
    run_to(provider, args, &mut std::io::stdout())
}

/// [`run`], writing to `out`.
pub fn run_to(
    provider: &dyn Provider,
    args: impl IntoIterator<Item = String>,
    out: &mut dyn Write,
) -> i32 {
    let mut args = args.into_iter();
    let (mut ids, mut kinds, mut allow) = (Vec::new(), Vec::new(), Vec::new());
    let (mut filter, mut limit) = (None, None);
    let (mut verbose, mut strict, mut compare_load) = (false, false, false);
    while let Some(a) = args.next() {
        let mut value = |what: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("{a} needs {what}"))
        };
        let step: Result<(), String> = match a.as_str() {
            "--id" => value("an id").map(|v| ids.push(v)),
            "--filter" => value("some text").map(|v| filter = Some(v)),
            "--kind" => value("a kind").and_then(|v| {
                parse_kind(&v)
                    .map(|k| kinds.push(k))
                    .ok_or_else(|| format!("unknown kind {v:?}"))
            }),
            "--limit" => value("a number").and_then(|v| match v.parse::<usize>() {
                Ok(0) => Err("--limit must be at least 1 (0 would check nothing)".to_owned()),
                Ok(n) => {
                    limit = Some(n);
                    Ok(())
                }
                Err(_) => Err(format!("{v:?} is not a number")),
            }),
            "--allow" => value("a code").map(|v| allow.push(v)),
            "--compare-load" => {
                compare_load = true;
                Ok(())
            }
            "--verbose" | "-v" => {
                verbose = true;
                Ok(())
            }
            "--strict" => {
                strict = true;
                Ok(())
            }
            "--help" | "-h" => {
                let _ = writeln!(out, "{USAGE}");
                return 0;
            }
            other => Err(format!("unknown argument {other:?}")),
        };
        if let Err(e) = step {
            let _ = writeln!(out, "check-conformance: {e}\n\n{USAGE}");
            return 2;
        }
    }

    // A filter becomes a list of ids, so the provider's own filtering decides what matches.
    if let Some(f) = &filter {
        match provider.list(f) {
            Ok(found) => ids.extend(found.into_iter().map(|a| a.id)),
            Err(e) => {
                let _ = writeln!(out, "check-conformance: listing failed: {e}");
                return 1;
            }
        }
        if ids.is_empty() {
            let _ = writeln!(out, "no asset matches {f:?}");
            return 0;
        }
    }
    let opts = ProviderOptions {
        compare_load,
        allow,
        ..ProviderOptions::default()
    };
    let min = if verbose {
        Severity::Info
    } else {
        Severity::Warning
    };
    let _ = writeln!(out, "{}: checking conformance", provider.game());

    let mut reports: Vec<Report> = Vec::new();
    let mut failed = false;
    let mut count = 0usize;
    // Reports are kept (they are small) for the summary; the assets themselves are not.
    let mut to_check = ids;
    if let Some(n) = limit {
        if to_check.is_empty() {
            if let Ok(all) = provider.list("") {
                to_check = all
                    .into_iter()
                    .filter(|a| kinds.is_empty() || kinds.contains(&a.kind))
                    .take(n)
                    .map(|a| a.id)
                    .collect();
            }
        } else {
            to_check.truncate(n);
        }
    }
    check_provider_with(provider, &to_check, &kinds, &opts, |_, r| {
        count += 1;
        let worst = r.worst();
        let tag = match worst {
            Some(Severity::Error) => "FAIL",
            Some(Severity::Warning) => "warn",
            _ => "ok  ",
        };
        failed |= worst == Some(Severity::Error) || (strict && worst == Some(Severity::Warning));
        let _ = writeln!(out, "[{tag}] {}", r.render(min));
        reports.push(r);
    });

    let _ = writeln!(out, "\n{count} asset(s) checked");
    let rows = tally(&reports);
    if !rows.is_empty() {
        let _ = writeln!(
            out,
            "{:<8} {:<40} {:>6} {:>7}  examples",
            "severity", "code", "count", "assets"
        );
        for t in rows.iter().filter(|t| t.severity >= min) {
            let _ = writeln!(
                out,
                "{:<8} {:<40} {:>6} {:>7}  {}",
                t.severity,
                t.code,
                t.count,
                t.assets,
                t.examples.join(", ")
            );
        }
    }
    i32::from(failed)
}
