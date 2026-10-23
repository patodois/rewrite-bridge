//! The documentation promises things the compiler cannot see: that the guide shows the example as it
//! is, and that the links between the README and the docs lead somewhere.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// docs/WRITING_A_PROVIDER.md section 5 is the whole of examples/cube_provider.rs, so the example
/// the CI builds and tests is the one a reader copies.
#[test]
fn the_guide_shows_the_cube_example_as_it_is() {
    let guide = read("docs/WRITING_A_PROVIDER.md");
    let example = read("examples/cube_provider.rs");
    assert!(
        guide.contains(example.trim_end()),
        "section 5 of docs/WRITING_A_PROVIDER.md differs from examples/cube_provider.rs: \
         paste the example into the guide again"
    );
}

/// The anchors GitHub gives the headings of a Markdown file: lower case, spaces to hyphens,
/// punctuation dropped.
fn anchors(text: &str) -> Vec<String> {
    let mut in_code = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        let hashes = line.chars().take_while(|&c| c == '#').count();
        if in_code || hashes == 0 || !line[hashes..].starts_with(' ') {
            continue;
        }
        out.push(
            line[hashes..]
                .trim()
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
                .map(|c| if c == ' ' { '-' } else { c })
                .collect(),
        );
    }
    out
}

/// Every relative link of the Markdown files points at a file or folder that exists, and a link to
/// a heading points at a heading that exists.
#[test]
fn relative_links_resolve() {
    for file in [
        "README.md",
        "docs/WRITING_A_PROVIDER.md",
        "docs/CONTRACT.md",
        "rewrite-bridge-bevy/README.md",
    ] {
        let text = read(file);
        let dir = Path::new(file).parent().unwrap();
        let mut in_code = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
            }
            if in_code {
                continue;
            }
            // `[text](target)`; a target with a scheme is external and not checked here
            for part in line.split("](").skip(1) {
                let Some(target) = part.split(')').next() else {
                    continue;
                };
                if target.contains("://") || target.starts_with("mailto:") {
                    continue;
                }
                let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
                let full = if path.is_empty() {
                    root().join(file)
                } else {
                    root().join(dir).join(path)
                };
                assert!(full.exists(), "{file}: the link ({target}) leads nowhere");
                if !anchor.is_empty() && full.extension().is_some_and(|e| e == "md") {
                    let headings = anchors(&std::fs::read_to_string(&full).unwrap());
                    assert!(
                        headings.iter().any(|h| h == anchor),
                        "{file}: the link ({target}) names a heading that does not exist"
                    );
                }
            }
        }
    }
}
