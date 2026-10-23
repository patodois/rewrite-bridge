//! Checking a whole provider: load assets one at a time, check each, drop it.

use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};

use super::report::{Location, Report, Severity};
use super::{check_animated, check_scene, scene, Expect};
use crate::{AssetInfo, AssetKind, NotSupported, Provider};

/// How [`check_provider_with`] checks.
#[derive(Clone, Debug)]
pub struct ProviderOptions {
    /// Also call `Provider::load` and require it to equal `load_animated(id)?.model`, as the
    /// trait asks. Loads every asset twice, so off by default.
    pub compare_load: bool,
    /// What to expect of each asset. The default is [`Expect::for_kind`] of its kind.
    pub expect: fn(&AssetInfo) -> Expect,
    /// Finding codes to drop everywhere, on top of each [`Expect::allow`].
    pub allow: Vec<String>,
}

impl Default for ProviderOptions {
    fn default() -> Self {
        Self {
            compare_load: false,
            expect: |info| Expect::for_kind(info.kind),
            allow: Vec::new(),
        }
    }
}

/// Loads and checks assets of a provider, one at a time.
///
/// `ids` names the assets (an empty slice means every asset the provider lists); `kinds` keeps
/// only assets of those kinds (an empty slice means all). Assets of kind [`AssetKind::Level`] are
/// loaded with `load_scene` and checked as scenes, the others with `load_animated`.
///
/// Each asset is loaded, checked and dropped before the next, so memory stays at one asset. A
/// failed load, or a provider that panics, is a finding on that asset, not a failure of the run.
pub fn check_provider<S: AsRef<str>>(
    provider: &dyn Provider,
    ids: &[S],
    kinds: &[AssetKind],
) -> Vec<(String, Report)> {
    let mut out = Vec::new();
    check_provider_with(
        provider,
        ids,
        kinds,
        &ProviderOptions::default(),
        |info, r| {
            out.push((info.id.clone(), r));
        },
    );
    out
}

/// [`check_provider`] with options, handing each report to `each` as soon as it is made instead of
/// collecting them.
pub fn check_provider_with<S: AsRef<str>>(
    provider: &dyn Provider,
    ids: &[S],
    kinds: &[AssetKind],
    opts: &ProviderOptions,
    mut each: impl FnMut(&AssetInfo, Report),
) {
    let mut known: Option<HashSet<String>> = None;
    let mut targets: Vec<(AssetInfo, bool)> = Vec::new();
    if ids.is_empty() {
        match provider.list("") {
            Ok(all) => targets.extend(all.into_iter().map(|a| (a, true))),
            Err(e) => {
                let info = AssetInfo {
                    id: "(list)".into(),
                    name: "list".into(),
                    kind: AssetKind::Other,
                };
                let mut r = Report::new("(list)");
                r.push(
                    Severity::Error,
                    "provider.list_failed",
                    Location::none(),
                    format!("Provider::list(\"\") failed: {e}"),
                );
                each(&info, r);
                return;
            }
        }
    } else {
        for id in ids {
            let id = id.as_ref();
            let found = provider
                .list(id)
                .ok()
                .and_then(|v| v.into_iter().find(|a| a.id == id));
            match found {
                Some(a) => targets.push((a, true)),
                None => targets.push((
                    AssetInfo {
                        id: id.to_string(),
                        name: id.to_string(),
                        kind: AssetKind::Other,
                    },
                    false,
                )),
            }
        }
    }
    for (info, listed) in targets {
        if !kinds.is_empty() && !kinds.contains(&info.kind) {
            continue;
        }
        let mut report = check_one(provider, &info, opts, &mut known);
        if !listed {
            report.push(
                Severity::Warning,
                "provider.unlisted_id",
                Location::none(),
                format!("Provider::list does not give the id {:?}", info.id),
            );
        }
        report.subject = info.id.clone();
        report.allow(&opts.allow);
        each(&info, report);
    }
}

fn check_one(
    provider: &dyn Provider,
    info: &AssetInfo,
    opts: &ProviderOptions,
    known: &mut Option<HashSet<String>>,
) -> Report {
    let mut expect = (opts.expect)(info);
    expect.allow.extend(opts.allow.iter().cloned());
    let result = catch_unwind(AssertUnwindSafe(|| {
        if info.kind == AssetKind::Level {
            match provider.load_scene(&info.id) {
                Ok(s) => {
                    let mut r = check_scene(&s, &expect);
                    if r.passed() {
                        let ids = known.get_or_insert_with(|| {
                            provider
                                .list("")
                                .map(|v| v.into_iter().map(|a| a.id).collect())
                                .unwrap_or_default()
                        });
                        if !ids.is_empty() {
                            scene::check_refs(&s, provider.game(), ids, &mut r);
                        }
                    }
                    r
                }
                Err(e) if e.downcast_ref::<NotSupported>().is_some() => {
                    let mut r = Report::new(&info.id);
                    r.push(
                        Severity::Info,
                        "provider.scene_unsupported",
                        Location::none(),
                        e.to_string(),
                    );
                    r
                }
                Err(e) => failed(&info.id, "load_scene", &e.to_string()),
            }
        } else {
            match provider.load_animated(&info.id) {
                Ok(a) => {
                    let mut r = check_animated(&a, &expect);
                    if opts.compare_load {
                        match provider.load(&info.id) {
                            Ok(m) if m == a.model => {}
                            Ok(_) => r.push(
                                Severity::Error,
                                "provider.load_mismatch",
                                Location::none(),
                                "load(id) differs from load_animated(id)?.model".into(),
                            ),
                            Err(e) => r.push(
                                Severity::Error,
                                "provider.load_failed",
                                Location::none(),
                                format!("load failed where load_animated worked: {e}"),
                            ),
                        }
                    }
                    r
                }
                Err(e) => failed(&info.id, "load_animated", &e.to_string()),
            }
        }
    }));
    match result {
        Ok(mut r) => {
            r.allow(&expect.allow);
            r
        }
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            let mut r = Report::new(&info.id);
            r.push(
                Severity::Error,
                "provider.panicked",
                Location::none(),
                format!("the provider panicked: {msg}"),
            );
            r
        }
    }
}

fn failed(id: &str, what: &str, err: &str) -> Report {
    let mut r = Report::new(id);
    r.push(
        Severity::Error,
        "provider.load_failed",
        Location::none(),
        format!("{what}({id:?}) failed: {err}"),
    );
    r
}
