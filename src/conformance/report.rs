//! What a conformance run returns: findings with a severity, a stable code and a location.

use std::collections::BTreeMap;
use std::fmt;

/// How bad a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Worth knowing, no action needed (a measurement, or a convention some games differ on).
    Info,
    /// Probably a mistake that a consumer will notice (a wrong scale, a spinning limb).
    Warning,
    /// The asset breaks the contract: a consumer may render it wrongly or fail.
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        })
    }
}

/// Where in the asset a finding is. Every field that does not apply is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Location {
    /// Index into `Model::nodes`.
    pub node: Option<usize>,
    /// Index into `Model::meshes` (a mesh is one primitive: one triangle list, one material).
    pub mesh: Option<usize>,
    /// Index into `Model::materials`.
    pub material: Option<usize>,
    /// Index into `Model::skins`.
    pub skin: Option<usize>,
    /// Name of the animation clip.
    pub clip: Option<String>,
    /// Index into the clip's channels.
    pub channel: Option<usize>,
    /// Index into `Scene::instances`.
    pub instance: Option<usize>,
    /// Index into `Scene::models`.
    pub model: Option<usize>,
}

impl Location {
    /// A location that points nowhere in particular (the asset as a whole).
    pub fn none() -> Self {
        Self::default()
    }
    pub fn mesh(i: usize) -> Self {
        Self {
            mesh: Some(i),
            ..Self::default()
        }
    }
    pub fn node(i: usize) -> Self {
        Self {
            node: Some(i),
            ..Self::default()
        }
    }
    pub fn material(i: usize) -> Self {
        Self {
            material: Some(i),
            ..Self::default()
        }
    }
    pub fn skin(i: usize) -> Self {
        Self {
            skin: Some(i),
            ..Self::default()
        }
    }
    pub fn clip(name: &str) -> Self {
        Self {
            clip: Some(name.to_string()),
            ..Self::default()
        }
    }
    pub fn instance(i: usize) -> Self {
        Self {
            instance: Some(i),
            ..Self::default()
        }
    }
    pub fn with_node(mut self, i: usize) -> Self {
        self.node = Some(i);
        self
    }
    pub fn with_channel(mut self, i: usize) -> Self {
        self.channel = Some(i);
        self
    }
    pub fn with_model(mut self, i: usize) -> Self {
        self.model = Some(i);
        self
    }
    /// True when no field is set.
    pub fn is_none(&self) -> bool {
        *self == Self::default()
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(i) = self.model {
            parts.push(format!("scene model {i}"));
        }
        if let Some(i) = self.instance {
            parts.push(format!("instance {i}"));
        }
        if let Some(c) = &self.clip {
            parts.push(format!("clip {c:?}"));
        }
        if let Some(i) = self.channel {
            parts.push(format!("channel {i}"));
        }
        if let Some(i) = self.mesh {
            parts.push(format!("mesh {i}"));
        }
        if let Some(i) = self.material {
            parts.push(format!("material {i}"));
        }
        if let Some(i) = self.skin {
            parts.push(format!("skin {i}"));
        }
        if let Some(i) = self.node {
            parts.push(format!("node {i}"));
        }
        f.write_str(&parts.join(", "))
    }
}

/// One thing the checker noticed.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub severity: Severity,
    /// Stable, dotted, lower-case identifier such as `scale.too_large`. Codes never change
    /// meaning, so providers can allow-list them; the list is in the [module docs](super).
    pub code: &'static str,
    /// For humans; its wording may change.
    pub message: String,
    pub location: Location,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:<7} {}", self.severity, self.code)?;
        if !self.location.is_none() {
            write!(f, " [{}]", self.location)?;
        }
        write!(f, " {}", self.message)
    }
}

/// Which way the model's front faces, when the model says so by a node or socket name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Facing {
    /// Nothing in the asset says which way the front is. The checker never guesses.
    #[default]
    Unknown,
    /// The bridge convention.
    PlusZ,
    MinusZ,
    PlusX,
    MinusX,
}

impl fmt::Display for Facing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Facing::Unknown => "unknown",
            Facing::PlusZ => "+Z",
            Facing::MinusZ => "-Z",
            Facing::PlusX => "+X",
            Facing::MinusX => "-X",
        })
    }
}

/// Numbers the checks measured, kept whether or not they produced a finding.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measurements {
    /// World-space minimum and maximum of the default pose (skins applied), in metres.
    pub bounds: Option<([f32; 3], [f32; 3])>,
    /// Facing, as far as node and socket names tell.
    pub facing: Facing,
    pub vertices: usize,
    pub triangles: usize,
    pub meshes: usize,
    pub clips: usize,
    pub instances: usize,
}

impl Measurements {
    /// Width (X), height (Y) and depth (Z) of [`Measurements::bounds`].
    pub fn size(&self) -> Option<[f32; 3]> {
        let (lo, hi) = self.bounds?;
        Some([hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]])
    }
}

/// The outcome of checking one asset.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// What was checked: an asset id, a model name.
    pub subject: String,
    pub findings: Vec<Finding>,
    pub measured: Measurements,
}

impl Report {
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            ..Self::default()
        }
    }

    pub(crate) fn push(
        &mut self,
        severity: Severity,
        code: &'static str,
        location: Location,
        message: String,
    ) {
        self.findings.push(Finding {
            severity,
            code,
            message,
            location,
        });
    }

    /// How many findings have exactly this severity.
    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    }

    /// The worst severity present, or `None` for a clean report.
    pub fn worst(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }

    /// No errors (warnings and info are allowed).
    pub fn passed(&self) -> bool {
        self.count(Severity::Error) == 0
    }

    /// No findings at all above info.
    pub fn is_clean(&self) -> bool {
        self.worst().is_none_or(|s| s == Severity::Info)
    }

    /// Findings with this code.
    pub fn with_code<'a>(&'a self, code: &'a str) -> impl Iterator<Item = &'a Finding> + 'a {
        self.findings.iter().filter(move |f| f.code == code)
    }

    /// True when some finding has this code.
    pub fn has(&self, code: &str) -> bool {
        self.with_code(code).next().is_some()
    }

    /// Drops findings whose code is in `allow` (conventions a game knowingly differs on).
    pub fn allow(&mut self, allow: &[String]) {
        if !allow.is_empty() {
            self.findings.retain(|f| !allow.iter().any(|a| a == f.code));
        }
    }

    /// Findings of at least `min` severity, worst first.
    pub fn at_least(&self, min: Severity) -> Vec<&Finding> {
        let mut v: Vec<&Finding> = self.findings.iter().filter(|f| f.severity >= min).collect();
        v.sort_by_key(|f| std::cmp::Reverse(f.severity));
        v
    }

    /// Like `Display` but leaves out findings below `min`.
    pub fn render(&self, min: Severity) -> String {
        let mut out = format!(
            "{}: {} error(s), {} warning(s), {} info",
            self.subject,
            self.count(Severity::Error),
            self.count(Severity::Warning),
            self.count(Severity::Info)
        );
        if let Some(s) = self.measured.size() {
            out.push_str(&format!(
                " | size {:.3} x {:.3} x {:.3} m (w x h x d), facing {}",
                s[0], s[1], s[2], self.measured.facing
            ));
        }
        for f in self.at_least(min) {
            out.push_str("\n  ");
            out.push_str(&f.to_string());
        }
        out
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(Severity::Info))
    }
}

/// How often a code showed up across many reports.
#[derive(Clone, Debug, PartialEq)]
pub struct Tally {
    pub code: &'static str,
    pub severity: Severity,
    /// Findings with this code, over all reports.
    pub count: usize,
    /// Reports that have at least one.
    pub assets: usize,
    /// Subjects of the first few reports that have it.
    pub examples: Vec<String>,
}

/// Groups the findings of many reports by code, worst and most frequent first.
pub fn tally<'a>(reports: impl IntoIterator<Item = &'a Report>) -> Vec<Tally> {
    let mut by_code: BTreeMap<&'static str, Tally> = BTreeMap::new();
    for r in reports {
        let mut seen: Vec<&'static str> = Vec::new();
        for f in &r.findings {
            let t = by_code.entry(f.code).or_insert_with(|| Tally {
                code: f.code,
                severity: f.severity,
                count: 0,
                assets: 0,
                examples: Vec::new(),
            });
            t.severity = t.severity.max(f.severity);
            t.count += 1;
            if !seen.contains(&f.code) {
                seen.push(f.code);
                t.assets += 1;
                if t.examples.len() < 3 {
                    t.examples.push(r.subject.clone());
                }
            }
        }
    }
    let mut v: Vec<Tally> = by_code.into_values().collect();
    v.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.assets.cmp(&a.assets))
            .then(a.code.cmp(b.code))
    });
    v
}
