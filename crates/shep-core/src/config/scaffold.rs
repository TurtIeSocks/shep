//! Building the commented Flockfile `shep init` writes, in every format shep can read.
//!
//! Builds the document uncommented, tagging each line as prose or code,
//! then comments it in one pass, so adding a format is a table entry
//! (`Syntax::of`) rather than a rewrite.
//!
//! Two comment styles, tested apart: prose is marker then a space; a
//! commented field is marker then the value, no space. Uncommenting strips
//! the marker from exactly the lines whose next character is not a space.
//!
//! JSON has no comment syntax, so [`Scaffold::build`] emits a live minimal
//! document there and refuses [`Depth::All`] rather than pin every default.

use core::fmt;

use crate::config::FlockFormat;

/// How much of the Flockfile grammar a scaffold shows.
///
/// Verbosity belongs to the moment rather than to the template: a newcomer
/// and a veteran want the same file at different depths.
///
/// Only [`Depth::All`] is machine-checkable. The drift test compares it
/// against the generated schema, which works precisely because that level is
/// meant to be exhaustive. [`Depth::Curated`] is editorial judgement about
/// what matters on day one, and no test can tell anyone it has gone stale,
/// so the friendly level is the expensive one to maintain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// The fields somebody needs on their first day. A file to read.
    Curated,
    /// Every option the grammar has, for somebody who knows what they want
    /// and cannot remember what it is called.
    All,
}

/// Why a scaffold could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScaffoldError {
    /// [`Depth::All`] was asked for in a format with no comments.
    ///
    /// Carries the format so the message can name it, and names `json5` as
    /// the way out, since it is JSON's syntax with comments added.
    NoCommentsForAll(FlockFormat),
}

impl fmt::Display for ScaffoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // `Syntax`'s own label rather than a `Display` on `FlockFormat`:
            // the human name of a format is already recorded once, and a
            // second spelling could drift from it.
            Self::NoCommentsForAll(format) => write!(
                f,
                "{} has no comment syntax, so a full scaffold would pin \
                 every default instead of explaining it; write a .json5 \
                 Flockfile for the same syntax with comments, or drop --all",
                Syntax::of(*format).label
            ),
        }
    }
}

impl core::error::Error for ScaffoldError {}

/// The fields [`Depth::Curated`] shows, in the order it shows them.
///
/// An explicit ordered list rather than a flag scattered across `AppConfig`'s
/// attributes, so membership and order stay one editorial decision, readable
/// at a glance. The order is a narrative: what is it, what runs, keep it
/// alive, where it runs.
///
/// Generation cannot supply this. schemars emits properties into a sorted
/// map, so a derived curated file would read `autorestart, cwd, name,
/// script`: alphabetical, and meaningless to somebody opening it first.
pub const CURATED: &[&str] = &["name", "script", "autorestart", "cwd"];

/// Group order for [`Depth::All`], coarsest concern first: what it is and
/// what runs, then what it receives, then how it is kept alive, then when.
///
/// Fields carrying no `group` sort after all of these: half of `AppConfig`
/// is currently ungrouped, so half the full scaffold is still alphabetical,
/// and leaving those at the end makes the gap visible instead of burying it
/// in the middle.
const GROUP_ORDER: &[&str] = &["process", "inputs", "control", "cron"];

/// One line of a scaffold, before any comment marker is applied.
///
/// The split is the whole trick: [`render`] prefixes prose with a marker and
/// a space, and code with a bare marker, which is what makes uncommenting
/// mechanical rather than a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// Explanation for a reader. Never uncommented, and dropped entirely by
    /// a format that cannot carry it.
    Prose(String),
    /// A real line of the document, commented out until somebody wants it.
    Code(String),
    /// A separator, emitted bare in every format.
    Blank,
}

/// One format's syntax, as data rather than as a branch per nesting level.
struct Syntax {
    /// Line comment marker, or `None` for a format that has none.
    marker: Option<&'static str>,
    /// What the preamble calls this format.
    label: &'static str,
    /// Lines that open the document and its one example app.
    open: &'static [&'static str],
    /// Prefix on each field line.
    indent: &'static str,
    /// Between a field's name and its value.
    separator: &'static str,
    /// Lines that close the document.
    close: &'static [&'static str],
    /// What follows every field but the last.
    ///
    /// JSON and JSON5 separate object members with a comma. TOML and YAML
    /// separate them with a newline and want nothing here, which is a
    /// different question from whether a trailing one is legal.
    member_sep: &'static str,
    /// Whether [`Syntax::member_sep`] may follow the last field too.
    ///
    /// JSON5 allows a trailing comma, so last-ness never has to be tracked
    /// there. Strict JSON does not.
    trailing_sep: bool,
    /// Whether field names are quoted.
    quoted_keys: bool,
}

impl Syntax {
    const fn of(format: FlockFormat) -> Self {
        match format {
            // `[[app]]` needs no closing line and no indent: a TOML array of
            // tables ends where the next one begins.
            FlockFormat::Toml => Self {
                marker: Some("#"),
                label: "TOML",
                open: &["[[app]]"],
                indent: "",
                separator: " = ",
                close: &[],
                member_sep: "",
                trailing_sep: false,
                quoted_keys: false,
            },
            // The lone `-` works because a sequence item whose value is a
            // block mapping on the following lines is valid YAML, so the
            // first field needs no special case for the dash.
            FlockFormat::Yaml => Self {
                marker: Some("#"),
                label: "YAML",
                open: &["app:", "  -"],
                indent: "    ",
                separator: ": ",
                close: &[],
                member_sep: "",
                trailing_sep: false,
                quoted_keys: false,
            },
            FlockFormat::Json5 => Self {
                marker: Some("//"),
                label: "JSON5",
                open: &["{", "  app: [", "    {"],
                indent: "      ",
                separator: ": ",
                close: &["    },", "  ],", "}"],
                member_sep: ",",
                trailing_sep: true,
                quoted_keys: false,
            },
            FlockFormat::Json => Self {
                marker: None,
                label: "JSON",
                open: &["{", "  \"app\": [", "    {"],
                indent: "      ",
                separator: ": ",
                close: &["    }", "  ]", "}"],
                member_sep: ",",
                trailing_sep: false,
                quoted_keys: true,
            },
        }
    }
}

/// A scaffold request: which format, and how much of the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scaffold {
    format: FlockFormat,
    depth: Depth,
}

impl Scaffold {
    /// A scaffold for `format` at `depth`.
    #[must_use]
    pub const fn new(format: FlockFormat, depth: Depth) -> Self {
        Self { format, depth }
    }

    /// The scaffold's text.
    ///
    /// Entirely commented out in a format that has comments, parsing as a
    /// document declaring no apps; a live minimal Flockfile in strict JSON,
    /// which has no comment to hide behind.
    ///
    /// # Errors
    /// - [`ScaffoldError::NoCommentsForAll`]: [`Depth::All`] in a format
    ///   with no comment syntax, where the result would pin every default.
    ///
    /// # Panics
    /// If a name in [`CURATED`] is not a field of `AppConfig`.
    #[track_caller]
    pub fn build(self) -> Result<String, ScaffoldError> {
        let syntax = Syntax::of(self.format);
        if syntax.marker.is_none() && self.depth == Depth::All {
            return Err(ScaffoldError::NoCommentsForAll(self.format));
        }
        Ok(render(&syntax, &document(&syntax, &self.field_names())))
    }

    /// The field names this scaffold shows, in the order it shows them.
    fn field_names(self) -> Vec<String> {
        match self.depth {
            Depth::Curated => CURATED.iter().map(|name| (*name).to_owned()).collect(),
            Depth::All => grouped_order(),
        }
    }
}

/// Every field name: the curated four first, then the rest by
/// [`GROUP_ORDER`] and alphabetically within each group.
///
/// The curated names lead because within a group the order is alphabetical,
/// which buried `name` and `script` at the ninth and twelfth lines of the
/// full scaffold. Those are the two fields `normalize` actually requires, so
/// a reader meeting the file for the first time should not have to hunt for
/// them. [`CURATED`] already records what matters first and in what order,
/// and reusing it here means one editorial decision rather than two that can
/// disagree.
fn grouped_order() -> Vec<String> {
    let schema = crate::config::flockfile_schema_json();
    let props = properties(&schema);

    let rank = |name: &str| -> usize {
        let group = props[name]["init"]["group"].as_str().unwrap_or_default();
        GROUP_ORDER
            .iter()
            .position(|known| *known == group)
            .unwrap_or(GROUP_ORDER.len())
    };

    // `props` is already alphabetical (schemars emits a sorted map), and a
    // stable sort by rank alone therefore leaves each group alphabetical.
    let mut rest: Vec<String> = props
        .keys()
        .filter(|name| !CURATED.contains(&name.as_str()))
        .cloned()
        .collect();
    rest.sort_by_key(|name| rank(name));

    let mut names: Vec<String> = CURATED.iter().map(|name| (*name).to_owned()).collect();
    names.extend(rest);
    names
}

/// `AppConfig`'s properties, as the schema describes them.
fn properties(schema: &schemars::Schema) -> &serde_json::Map<String, serde_json::Value> {
    schema
        .pointer("#/$defs/AppConfig/properties")
        .expect("app config properties must exist")
        .as_object()
        .expect("props must be an object")
}

/// The document a format would accept, uncommented, one [`Line`] per line.
///
/// This is the whole scaffold as a real Flockfile. Nothing here knows what a
/// comment is.
#[track_caller]
fn document(syntax: &Syntax, names: &[String]) -> Vec<Line> {
    let schema = crate::config::flockfile_schema_json();
    let props = properties(&schema);

    let mut lines = Vec::new();
    if syntax.marker.is_some() {
        lines.push(Line::Prose("Manage your app in a Flockfile".to_owned()));
        lines.push(Line::Prose(format!(
            "Add as many apps as you would like using {} syntax",
            syntax.label
        )));
        lines.push(Line::Blank);
    }
    for line in syntax.open {
        lines.push(Line::Code((*line).to_owned()));
    }

    for (index, name) in names.iter().enumerate() {
        let field = props
            .get(name)
            .unwrap_or_else(|| panic!("`{name}` is not a field of AppConfig"));

        if syntax.marker.is_some() {
            for line in blurb(name, field).lines() {
                lines.push(Line::Prose(line.to_owned()));
            }
        }

        let last = index + 1 == names.len();
        let comma = if last && !syntax.trailing_sep {
            ""
        } else {
            syntax.member_sep
        };
        let key = if syntax.quoted_keys {
            format!("\"{name}\"")
        } else {
            name.clone()
        };
        lines.push(Line::Code(format!(
            "{}{key}{}{}{comma}",
            syntax.indent,
            syntax.separator,
            literal(syntax, field),
        )));
    }

    for line in syntax.close {
        lines.push(Line::Code((*line).to_owned()));
    }
    lines
}

/// Puts `syntax`'s comment marker on, and nothing else.
///
/// Prose gets the marker and a space; code gets the marker alone. A format
/// with no marker drops prose entirely and emits code bare, which is what
/// makes strict JSON's live document fall out of the same builder rather
/// than needing one of its own.
fn render(syntax: &Syntax, lines: &[Line]) -> String {
    let mut out = String::new();
    for line in lines {
        match (syntax.marker, line) {
            (_, Line::Blank) => {}
            (None, Line::Prose(_)) => continue,
            (None, Line::Code(code)) => out.push_str(code),
            (Some(marker), Line::Prose(text)) => {
                out.push_str(marker);
                out.push(' ');
                out.push_str(text);
            }
            // The marker goes after the line's own indentation, not before
            // it: a nested format's code is indented, and a marker written
            // first would be followed by a space and read as prose.
            (Some(marker), Line::Code(code)) => {
                let content = code.trim_start_matches(' ');
                let indent = &code[..code.len() - content.len()];
                out.push_str(indent);
                out.push_str(marker);
                out.push_str(content);
            }
        }
        out.push('\n');
    }
    out
}

/// What a field's line should explain.
///
/// Takes `init.blurb` rather than the `///` doc: the doc is written for
/// somebody reading the source, and cites internal type names and spec
/// section numbers a Flockfile reader would not recognize.
///
/// # Panics
/// If `field` has no `init.blurb`. Falling back to the `///` doc would put
/// source-facing prose in a file that otherwise reads as documentation;
/// `every_field_carries_a_group_and_a_blurb` should make this unreachable.
#[track_caller]
fn blurb(name: &str, field: &serde_json::Value) -> String {
    field["init"]
        .as_object()
        .and_then(|init| init.get("blurb"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("`{name}` has no `init.blurb`; add one in config/app.rs"))
        .to_owned()
}

/// A field's placeholder value, written the way `syntax` spells literals.
///
/// A field's schema `default` is only usable when it is both present and
/// non-empty: `Option<T>` fields serialize their `None` as `null`, but a
/// required `String` field still gets a `default` from `#[serde(default)]`
/// at the struct level, holding `String::new()`. That empty string is not a
/// value anyone would want uncommented, so it is treated the same as no
/// default at all, and both fall through to `init.example`.
fn literal(syntax: &Syntax, field: &serde_json::Value) -> String {
    let has_no_real_default = field["default"].is_null() || field["default"].as_str() == Some("");
    let value = if has_no_real_default {
        field["init"]
            .as_object()
            .and_then(|init| init.get("example"))
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(String::new()))
    } else {
        field["default"].clone()
    };

    // JSON's literal grammar is a subset of YAML's and of JSON5's, so one
    // rendering serves three of the four formats. TOML is the odd one:
    // `toml::Value`'s Display is what knows to write an array inline and a
    // string with TOML's own escaping.
    if syntax.separator == " = " {
        toml::Value::try_from(&value)
            .expect("a schema example must be representable as TOML")
            .to_string()
    } else {
        serde_json::to_string(&value).expect("a serde_json value re-serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Flockfile, FlockfileError};

    /// The three formats whose scaffold is a commented template.
    const COMMENTED: [FlockFormat; 3] = [FlockFormat::Toml, FlockFormat::Yaml, FlockFormat::Json5];
    const DEPTHS: [Depth; 2] = [Depth::Curated, Depth::All];

    /// Strips `marker` from exactly the lines whose next character is not a
    /// space, which is what a reader does by hand.
    ///
    /// The marker sits after any indentation, so this has to look past the
    /// leading spaces to find it and then put them back.
    fn uncomment(text: &str, marker: &str) -> String {
        text.lines()
            .map(|line| {
                let trimmed = line.trim_start_matches(' ');
                let indent = &line[..line.len() - trimmed.len()];
                match trimmed.strip_prefix(marker) {
                    Some(rest) if !rest.starts_with(' ') => format!("{indent}{rest}"),
                    _ => line.to_owned(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn marker_of(format: FlockFormat) -> &'static str {
        Syntax::of(format)
            .marker
            .expect("a commented format has a marker")
    }

    #[test]
    fn every_commented_format_uncomments_into_a_working_flockfile() {
        for format in COMMENTED {
            for depth in DEPTHS {
                let scaffold = Scaffold::new(format, depth).build().expect("builds");
                let live = uncomment(&scaffold, marker_of(format));

                let parsed = Flockfile::parse(&live, format).unwrap_or_else(|err| {
                    panic!(
                        "the uncommented {format:?} scaffold at {depth:?} must parse: {err}\n\
                         --- what was parsed ---\n{live}"
                    )
                });

                assert_eq!(parsed.apps.len(), 1, "{format:?}/{depth:?}:\n{live}");
                assert!(
                    !parsed.apps[0].name.is_empty(),
                    "{format:?}/{depth:?} needs a name"
                );
                assert!(
                    !parsed.apps[0].script.is_empty(),
                    "{format:?}/{depth:?} needs a script"
                );
            }
        }
    }

    #[test]
    fn a_commented_scaffold_never_declares_an_app_until_somebody_uncomments_it() {
        // TOML and YAML read a comments-only file as an empty document and
        // parse with no apps; JSON5 requires a value, so an all-comments
        // file refuses at the parser instead.
        for format in COMMENTED {
            let scaffold = Scaffold::new(format, Depth::Curated)
                .build()
                .expect("builds");
            match Flockfile::parse(&scaffold, format) {
                Err(FlockfileError::NoApps) => {
                    assert_ne!(
                        format,
                        FlockFormat::Json5,
                        "json5 cannot parse a valueless file"
                    );
                }
                Err(_) => assert_eq!(
                    format,
                    FlockFormat::Json5,
                    "only json5 refuses a comments-only file at the parser:\n{scaffold}"
                ),
                Ok(flock) => panic!(
                    "{format:?} handed back {} apps from a template nobody has \
                     uncommented:\n{scaffold}",
                    flock.apps.len()
                ),
            }
        }
    }

    #[test]
    fn the_json_scaffold_is_live_because_json_cannot_carry_guidance() {
        let scaffold = Scaffold::new(FlockFormat::Json, Depth::Curated)
            .build()
            .expect("json builds at the curated depth");

        let parsed = Flockfile::parse(&scaffold, FlockFormat::Json)
            .unwrap_or_else(|err| panic!("the json scaffold parses as written: {err}\n{scaffold}"));
        assert_eq!(parsed.apps.len(), 1);
        assert!(!parsed.apps[0].name.is_empty());
        assert!(!parsed.apps[0].script.is_empty());
        assert!(!scaffold.contains('#'), "json has no comments to write");
    }

    #[test]
    fn json_refuses_the_full_depth_and_points_at_json5() {
        let err = Scaffold::new(FlockFormat::Json, Depth::All)
            .build()
            .expect_err("all forty fields in json would pin every default");
        let shown = err.to_string();
        assert!(shown.contains("JSON"), "{shown}");
        assert!(
            shown.contains("json5"),
            "the way out has to be named: {shown}"
        );
    }

    #[test]
    fn the_all_depth_names_every_option_the_schema_knows() {
        let schema = crate::config::flockfile_schema_json();
        let props = properties(&schema);

        for format in COMMENTED {
            let text = Scaffold::new(format, Depth::All).build().expect("builds");
            let missing: Vec<&String> = props
                .keys()
                .filter(|f| !text.contains(f.as_str()))
                .collect();
            assert!(
                missing.is_empty(),
                "--all must name every option the grammar has; {format:?} is missing {}: {missing:?}",
                missing.len()
            );
        }
    }

    #[test]
    fn every_field_carries_a_group_and_a_blurb() {
        // A field with no `group` sorts after every grouped one; a field
        // with no `blurb` silently falls back to its `///` doc, written for
        // somebody reading the source.
        let schema = crate::config::flockfile_schema_json();
        let props = properties(&schema);

        let mut faults: Vec<String> = Vec::new();
        for (name, field) in props {
            let init = field["init"].as_object();
            let group = init.and_then(|i| i.get("group")).and_then(|g| g.as_str());
            let blurb = init.and_then(|i| i.get("blurb")).and_then(|b| b.as_str());

            match group {
                None => faults.push(format!("{name}: no `group`")),
                Some(group) if !GROUP_ORDER.contains(&group) => {
                    faults.push(format!(
                        "{name}: unknown group {group:?}, expected one of {GROUP_ORDER:?}"
                    ));
                }
                Some(_) => {}
            }
            match blurb {
                None => faults.push(format!("{name}: no `blurb`")),
                Some(blurb) if blurb.trim().is_empty() => {
                    faults.push(format!("{name}: empty `blurb`"));
                }
                // The scaffold puts these in a column of comments, so they
                // are consistent or they look broken. No dash anywhere a
                // person reads is a project-wide rule; the missing full stop
                // is the house style the first five set.
                Some(blurb) if blurb.contains('\u{2014}') || blurb.contains('\u{2013}') => {
                    faults.push(format!("{name}: `blurb` has a dash in it"));
                }
                Some(blurb) if blurb.trim_end().ends_with('.') => {
                    faults.push(format!(
                        "{name}: `blurb` ends with a full stop; the others do not"
                    ));
                }
                Some(_) => {}
            }
        }

        assert!(
            faults.is_empty(),
            "every AppConfig field needs `init.group` and `init.blurb`, set with \
             #[cfg_attr(feature = \"schema\", schemars(extend(\"init\" = {{ .. }})))] \
             in config/app.rs:\n  {}",
            faults.join("\n  ")
        );
    }

    #[test]
    fn every_curated_field_is_a_real_field() {
        let schema = crate::config::flockfile_schema_json();
        let props = properties(&schema);
        for name in CURATED {
            assert!(
                props.contains_key(*name),
                "`{name}` is not a field of AppConfig"
            );
        }
    }

    #[test]
    fn the_curated_depth_stays_short() {
        // Nothing else pins that Curated is short, so a swapped match arm
        // could hand a newcomer all forty options and no test would notice.
        for format in COMMENTED {
            let text = Scaffold::new(format, Depth::Curated)
                .build()
                .expect("builds");
            let schema = crate::config::flockfile_schema_json();
            let named = properties(&schema)
                .keys()
                .filter(|f| text.contains(f.as_str()))
                .count();
            assert!(
                named <= CURATED.len() + 2,
                "{format:?}'s curated scaffold names {named} fields; it is meant to show {}",
                CURATED.len()
            );
        }
    }
}
