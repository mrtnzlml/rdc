//! Splices named marker regions into a text file, leaving every other byte
//! alone. `rdc` uses this for more than one file kind, each needing its own
//! comment syntax so the markers stay invisible to that file's renderer:
//!
//! - `.gitlab-ci.yml` ([`YAML`]): `# >>> rdc:<name>` / `# <<< rdc:<name>`, an
//!   ordinary YAML/shell comment.
//! - Markdown docs ([`MARKDOWN`]): `<!-- >>> rdc:<name> -->` /
//!   `<!-- <<< rdc:<name> -->`, a real HTML comment — a bare `# >>>
//!   rdc:<name>` would render as a Markdown heading.
//!
//! The two styles are kept isolated from each other: a marker written in one
//! style is invisible when scanned in the other, so a YAML pipeline can never
//! be spliced with a Markdown body, or vice versa.

use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, BTreeSet};

/// The delimiters wrapping a region marker's `>>> rdc:<name>` /
/// `<<< rdc:<name>` body.
#[derive(Debug, Clone, Copy)]
pub struct MarkerStyle {
    pub prefix: &'static str,
    pub suffix: &'static str,
}

/// `# >>> rdc:<name>` — an ordinary YAML/shell comment.
pub const YAML: MarkerStyle = MarkerStyle { prefix: "# ", suffix: "" };

/// `<!-- >>> rdc:<name> -->` — a real HTML comment, so the marker doesn't
/// render as Markdown (a bare `# >>> rdc:<name>` would become an `<h1>`).
pub const MARKDOWN: MarkerStyle = MarkerStyle { prefix: "<!-- ", suffix: " -->" };

/// Names of the rdc regions whose opening marker appears in `existing`.
pub fn regions_present(existing: &str, style: MarkerStyle) -> BTreeSet<String> {
    existing.lines().filter_map(|l| marker_name(l, ">>>", style)).collect()
}

/// If `line` is an rdc region marker of `kind` (`">>>"` or `"<<<"`) in the
/// given `style`, its region name. The marker may carry a trailing comment,
/// and it may be indented. The closing delimiter (if the style has one) is
/// stripped before the name is read, so a suffix hugging the name with no
/// separating space (`<!-- >>> rdc:envs-->`) still parses.
pub(crate) fn marker_name(line: &str, kind: &str, style: MarkerStyle) -> Option<String> {
    let rest = line.trim_start().strip_prefix(&format!("{}{kind} rdc:", style.prefix))?;
    let closing = style.suffix.trim();
    let rest = if closing.is_empty() {
        rest
    } else {
        rest.strip_suffix(closing).unwrap_or(rest)
    };
    let name: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    if name.is_empty() {
        return None;
    }
    Some(format!("rdc:{name}"))
}

/// The current body of one rdc region in `existing`, with the marker's own
/// indentation stripped from each line (so it round-trips through
/// [`splice`], which re-applies that indentation).
///
/// `None` when the region is absent, or opened and never closed -- both cases
/// [`splice`] either ignores or diagnoses, so there is nothing to report here.
/// Used by callers that build a region body *from* what the file already says
/// rather than purely from `rdc.toml`: the deploy-jobs region is only half
/// derived (rdc knows the job name, the user supplies `RDC_SRC`), so its
/// existing content has to survive verbatim.
pub fn region_body(existing: &str, name: &str, style: MarkerStyle) -> Option<String> {
    let mut indent = String::new();
    let mut body: Vec<&str> = Vec::new();
    let mut open = false;
    for line in existing.lines() {
        if !open {
            if marker_name(line, ">>>", style).as_deref() == Some(name) {
                indent = line.chars().take_while(|c| c.is_whitespace()).collect();
                open = true;
            }
            continue;
        }
        if marker_name(line, "<<<", style).as_deref() == Some(name) {
            return Some(
                body.iter()
                    .map(|l| l.strip_prefix(indent.as_str()).unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        body.push(line);
    }
    None
}

/// Replace the body of every rdc region in `existing` with the rendered one,
/// leaving every other byte alone.
///
/// `Ok(None)` means the file carries no rdc markers — a hand-written file,
/// which is left untouched rather than converted. Every error case (a region
/// never closed, closed without opening, duplicated, nested, or misspelled)
/// returns `Err` before producing any output, so the caller has nothing to
/// write: a half-spliced file is worse than a diagnosed one.
pub fn splice(
    existing: &str,
    regions: &BTreeMap<&str, String>,
    style: MarkerStyle,
) -> Result<Option<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // (region name, line it opened on, its indentation)
    let mut open: Option<(String, usize, String)> = None;
    let mut any = false;

    for (idx, line) in existing.lines().enumerate() {
        let lineno = idx + 1;

        if let Some(name) = marker_name(line, ">>>", style) {
            let Some(body) = regions.get(name.as_str()) else {
                return Err(anyhow!(
                    "line {lineno}: unknown rdc region '{name}' (known: {})",
                    regions.keys().copied().collect::<Vec<_>>().join(", ")
                ));
            };
            if let Some((open_name, open_line, _)) = &open {
                return Err(anyhow!(
                    "line {lineno}: region '{name}' opens while '{open_name}' from \
                     line {open_line} is still open"
                ));
            }
            if !seen.insert(name.clone()) {
                return Err(anyhow!(
                    "line {lineno}: region '{name}' appears more than once"
                ));
            }
            any = true;
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            out.push(line.to_string());
            for body_line in body.lines() {
                if body_line.is_empty() {
                    out.push(String::new());
                } else {
                    out.push(format!("{indent}{body_line}"));
                }
            }
            open = Some((name, lineno, indent));
            continue;
        }

        if let Some(name) = marker_name(line, "<<<", style) {
            match &open {
                Some((open_name, _, _)) if *open_name == name => {
                    out.push(line.to_string());
                    open = None;
                }
                Some((open_name, open_line, _)) => {
                    return Err(anyhow!(
                        "line {lineno}: region '{name}' closes while '{open_name}' \
                         from line {open_line} is open"
                    ));
                }
                None => {
                    return Err(anyhow!(
                        "line {lineno}: region '{name}' closes without opening"
                    ));
                }
            }
            continue;
        }

        // Lines inside an open region are the previously generated body: dropped.
        if open.is_none() {
            out.push(line.to_string());
        }
    }

    if let Some((name, lineno, _)) = open {
        return Err(anyhow!(
            "region '{name}' opened at line {lineno} is never closed by '{}<<< {name}{}'",
            style.prefix, style.suffix
        ));
    }
    if !any {
        return Ok(None);
    }
    let mut joined = out.join("\n");
    // `lines()` drops the final newline; put it back only if it was there.
    if existing.ends_with('\n') {
        joined.push('\n');
    }
    Ok(Some(joined))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::gitlab_ci::{render_regions, test_envs as envs};
    use std::collections::BTreeMap;

    /// A single region with fixed, recognizable content -- used by the tests
    /// below that exercise style-specific marker syntax directly, where the
    /// assertion is about the marker text rather than a real rendered body.
    fn one_region() -> BTreeMap<&'static str, String> {
        BTreeMap::from([("rdc:envs", "fresh line one\nfresh line two".to_string())])
    }

    const FILE: &str = "\
before
  parallel:
    matrix:
      # >>> rdc:archive-envs  (generated)
      - RDC_ENV: \"stale\"
      # <<< rdc:archive-envs
middle
# >>> rdc:deploy-jobs  (generated)
stale
# <<< rdc:deploy-jobs
after
";

    #[test]
    fn splice_replaces_bodies_and_keeps_everything_else() {
        let out = splice(FILE, &render_regions(&envs(&["dev", "test"])), YAML)
            .unwrap()
            .unwrap();
        assert!(out.starts_with("before\n  parallel:\n    matrix:\n"));
        assert!(out.ends_with("# <<< rdc:deploy-jobs\nafter\n"));
        assert!(out.contains("\nmiddle\n"));
        assert!(!out.contains("stale"));
        // the marker lines themselves, comments included, survive verbatim
        assert!(out.contains("      # >>> rdc:archive-envs  (generated)\n"));
    }

    #[test]
    fn splice_indents_a_body_to_its_marker() {
        let out = splice(FILE, &render_regions(&envs(&["dev"])), YAML).unwrap().unwrap();
        assert!(out.contains("\n      - RDC_ENV: \"dev\"\n"));
        assert!(out.contains("\n        RDC_VAR_SUFFIX: \"DEV\"\n"));
    }

    #[test]
    fn splice_leaves_a_hand_written_pipeline_alone() {
        let hand = "stages:\n  - test\npytest:\n  script:\n    - pytest -q\n";
        assert!(splice(hand, &render_regions(&envs(&["dev"])), YAML).unwrap().is_none());
    }

    #[test]
    fn splice_preserves_a_missing_trailing_newline() {
        let no_nl = FILE.trim_end_matches('\n');
        let out = splice(no_nl, &render_regions(&envs(&["dev"])), YAML).unwrap().unwrap();
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn splice_rejects_a_region_that_is_never_closed() {
        let broken = "# >>> rdc:deploy-jobs\nbody\n";
        let err =
            format!("{:#}", splice(broken, &render_regions(&envs(&["dev"])), YAML).unwrap_err());
        assert!(err.contains("rdc:deploy-jobs"), "{err}");
        assert!(err.contains("never closed"), "{err}");
    }

    #[test]
    fn splice_rejects_a_close_without_an_open() {
        let broken = "# <<< rdc:deploy-jobs\n";
        let err =
            format!("{:#}", splice(broken, &render_regions(&envs(&["dev"])), YAML).unwrap_err());
        assert!(err.contains("without opening"), "{err}");
    }

    #[test]
    fn splice_rejects_a_duplicated_region() {
        let broken = "# >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n\
                      # >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n";
        let err =
            format!("{:#}", splice(broken, &render_regions(&envs(&["dev"])), YAML).unwrap_err());
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn splice_rejects_a_misspelled_region() {
        // Silently copying it would mean that region never updates again.
        let broken = "# >>> rdc:archive-env\n# <<< rdc:archive-env\n";
        let err =
            format!("{:#}", splice(broken, &render_regions(&envs(&["dev"])), YAML).unwrap_err());
        assert!(err.contains("unknown rdc region"), "{err}");
        // The `regions.keys()` change (replacing the old REGIONS const) must
        // still populate the "known: ..." list from the caller's map.
        assert!(err.contains("rdc:archive-envs"), "the error must list what IS known: {err}");
    }

    #[test]
    fn splice_rejects_a_region_opened_inside_another() {
        let broken = "# >>> rdc:deploy-jobs\n# >>> rdc:archive-envs\n\
                      # <<< rdc:archive-envs\n# <<< rdc:deploy-jobs\n";
        let err =
            format!("{:#}", splice(broken, &render_regions(&envs(&["dev"])), YAML).unwrap_err());
        assert!(err.contains("still open"), "{err}");
    }

    #[test]
    fn splice_is_idempotent() {
        let regions = render_regions(&envs(&["dev", "test"]));
        let once = splice(FILE, &regions, YAML).unwrap().unwrap();
        let twice = splice(&once, &regions, YAML).unwrap().unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn region_body_returns_the_current_body_de_indented() {
        assert_eq!(
            region_body(FILE, "rdc:archive-envs", YAML).unwrap(),
            "- RDC_ENV: \"stale\""
        );
        assert_eq!(region_body(FILE, "rdc:deploy-jobs", YAML).unwrap(), "stale");
        // absent, and opened-but-never-closed, both read as "nothing to keep"
        assert!(region_body(FILE, "rdc:envs", YAML).is_none());
        assert!(region_body("# >>> rdc:deploy-jobs\nbody\n", "rdc:deploy-jobs", YAML).is_none());
    }

    /// What [`region_body`] returns must survive a splice unchanged, or the
    /// additive deploy region would churn the file on every run.
    #[test]
    fn region_body_round_trips_through_splice() {
        let body = region_body(FILE, "rdc:deploy-jobs", YAML).unwrap();
        let regions = BTreeMap::from([
            ("rdc:archive-envs", region_body(FILE, "rdc:archive-envs", YAML).unwrap()),
            ("rdc:deploy-jobs", body),
        ]);
        assert_eq!(splice(FILE, &regions, YAML).unwrap().unwrap(), FILE);
    }

    #[test]
    fn a_yaml_marker_is_invisible_under_the_markdown_style() {
        // Keeps a YAML pipeline from ever being spliced with a Markdown body.
        assert!(regions_present(FILE, MARKDOWN).is_empty());
    }

    #[test]
    fn a_markdown_marker_is_invisible_under_the_yaml_style() {
        // ...and a Markdown doc from ever being spliced with a YAML body.
        let doc = "before\n<!-- >>> rdc:envs -->\nstale\n<!-- <<< rdc:envs -->\nafter\n";
        assert!(regions_present(doc, YAML).is_empty());
    }

    #[test]
    fn markdown_markers_are_html_comments_not_headings() {
        // `# >>> rdc:envs` would render as an H1 in Markdown.
        let doc = "# Title\n\n<!-- >>> rdc:envs (generated) -->\nstale\n\
                   <!-- <<< rdc:envs -->\n\nkeep me\n";
        let out = splice(doc, &one_region(), MARKDOWN).unwrap().unwrap();
        assert!(out.contains("fresh line one\nfresh line two\n"));
        assert!(!out.contains("stale"));
        assert!(out.starts_with("# Title\n"));
        assert!(out.ends_with("keep me\n"));
        assert!(out.contains("<!-- >>> rdc:envs (generated) -->"));
    }

    #[test]
    fn a_markdown_marker_with_no_space_before_the_close_still_parses() {
        let doc = "<!-- >>> rdc:envs-->\nstale\n<!-- <<< rdc:envs-->\n";
        let out = splice(doc, &one_region(), MARKDOWN).unwrap().unwrap();
        assert!(out.contains("fresh line one"));
        assert!(!out.contains("stale"));
    }

    #[test]
    fn markdown_style_parses_the_closing_delimiter_with_or_without_a_space() {
        let with_space = "<!-- >>> rdc:envs -->\nbody\n<!-- <<< rdc:envs -->\n";
        let without_space = "<!-- >>> rdc:envs-->\nbody\n<!-- <<< rdc:envs-->\n";
        assert_eq!(
            regions_present(with_space, MARKDOWN),
            regions_present(without_space, MARKDOWN)
        );
        assert!(regions_present(with_space, MARKDOWN).contains("rdc:envs"));
    }
}
