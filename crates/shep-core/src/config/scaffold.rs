//! Building the commented Flockfile `shep init` writes, in every format
//! shep can read.
//!
//! # Why this lives here and not in the CLI
//!
//! A scaffold is a Flockfile that has not been filled in yet, so it belongs
//! beside the grammar it is a specimen of. Every fact it needs -- which
//! fields exist, what each is for, what a plausible value looks like -- comes
//! from the same schema [`crate::config::flockfile_schema_json`] serves, and
//! the document shapes it emits are the ones
//! [`crate::config::Flockfile::parse`] accepts two hundred lines up. Putting
//! the generator anywhere else would mean a second copy of the grammar's
//! shape, kept in step by hand.
//!
//! # The one idea that makes four formats tractable
//!
//! A scaffold is almost entirely commented out: the reader uncomments the
//! lines they want. The obvious way to build one is to write each format's
//! commented text directly, and it does not work -- the marker ends up
//! threaded through every structural fragment, so `app:` and `[` and `{` all
//! have to carry it, and each nesting level becomes its own special case.
//!
//! So this module **builds the document uncommented, then comments it in one
//! final pass.** Every line is tagged as prose or as code, one step emits the
//! real Flockfile a format would accept, and a second step puts the marker
//! on. The marker never touches the structure, which is why adding a format
//! is a table entry rather than a rewrite.
//!
//! # The comment convention, which is load-bearing
//!
//! Two kinds of comment line, and a test relies on telling them apart:
//!
//! - **Prose**, for a reader: marker then a SPACE. `# Every app gets one.`
//! - **Commented-out config**, meant to be uncommented: marker then the
//!   config, no space. `#name = "api"`
//!
//! Uncommenting therefore means stripping the marker from exactly the lines
//! whose next character is not a space, which is what lets a test uncomment
//! each format mechanically and prove the result parses -- that the scaffold
//! is a real Flockfile rather than plausible-looking prose.
//!
//! # Strict JSON is the exception, and it cannot be argued away
//!
//! JSON has no comment syntax. Not an awkward one -- none. So the product
//! this module exists to make cannot be made in it, and [`Scaffold::build`]
//! emits a live minimal document there instead: real values, no guidance.
//! [`Depth::All`] is refused for JSON rather than fudged, because a JSON
//! document naming all forty fields would pin every default explicitly,
//! which is a Flockfile you would tell somebody not to commit. Rin's call,
//! 2026-08-23. `.json5` is the format with JSON's syntax and comments, and
//! the refusal says so.

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
/// attributes, because membership and ORDER are one editorial decision and
/// belong somewhere a person can read at a glance. The order is a narrative:
/// what is it, what runs, keep it alive, where it runs.
///
/// Generation cannot supply this. schemars emits properties into a sorted
/// map, so a derived curated file would read `autorestart, cwd, name,
/// script`: alphabetical, and meaningless to somebody opening it first.
pub const CURATED: &[&str] = &["name", "script", "autorestart", "cwd"];

/// Group order for [`Depth::All`], coarsest concern first: what it is and
/// what runs, then what it receives, then how it is kept alive, then when.
///
/// Fields carrying no `group` sort after all of these. That is deliberate
/// rather than tidy: half of `AppConfig` is currently ungrouped, so half the
/// full scaffold is still alphabetical, and leaving those at the end makes
/// the gap visible instead of hiding it in the middle.
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
    /// different question from whether a TRAILING one is legal.
    member_sep: &'static str,
    /// Whether [`Syntax::member_sep`] may follow the LAST field too.
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
            // The lone `-` is deliberate. A sequence item whose value is a
            // block mapping on the following lines is valid YAML, and
            // writing it that way means the first field needs no special
            // case for the dash.
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
    /// In a format that has comments the result is entirely commented out,
    /// so it parses as a document declaring no apps until somebody
    /// uncomments a line. In strict JSON it is a live minimal Flockfile,
    /// because there is no comment to hide behind.
    ///
    /// # Errors
    /// - [`ScaffoldError::NoCommentsForAll`] -- [`Depth::All`] in a format
    ///   with no comment syntax, where the result would pin every default
    ///   rather than explain it.
    ///
    /// # Panics
    /// If a name in [`CURATED`] is not a field of `AppConfig`. That is a
    /// build-time editorial mistake, not a runtime condition, and there is a
    /// test that fails first.
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
            for line in blurb(field).lines() {
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
            // The marker goes AFTER the line's own indentation, not before
            // it. Prose is "marker then a space" and code is "marker then
            // content", which is what makes uncommenting mechanical -- but a
            // nested format indents its code, so a marker written first
            // would be followed by a space and would read as prose. Putting
            // the indentation outside keeps the marker glued to real
            // content at every depth, and uncommenting leaves the
            // indentation exactly where the document needs it.
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

/// What a field's line should explain, preferring the operator-facing blurb.
///
/// `init.blurb` is preferred over the `///` doc because the two have
/// different readers: several of `AppConfig`'s docs cite internal types and
/// spec sections, which mean nothing to somebody editing a Flockfile.
fn blurb(field: &serde_json::Value) -> String {
    field["init"]
        .as_object()
        .and_then(|init| init.get("blurb"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            field["description"]
                .as_str()
                .expect("description must be present")
                .to_owned()
        })
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
        // The file shep writes is a template, not a running configuration,
        // so none of these may hand back an app. HOW they decline differs by
        // language and the difference is real rather than a wart worth
        // hiding: TOML and YAML both read a comments-only file as an empty
        // document, so they parse and find nothing. JSON5 requires a value,
        // and a file that is entirely comments does not contain one, so it
        // refuses at the parser.
        //
        // Both readings say the same thing to an operator who ran `shep
        // start` on a template they had not filled in yet, which is the only
        // way anybody meets this.
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
        // The mirror of the drift test above. Nothing else pins that
        // Curated is SHORT, so a swapped match arm could hand a newcomer all
        // forty options and no test would notice.
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
