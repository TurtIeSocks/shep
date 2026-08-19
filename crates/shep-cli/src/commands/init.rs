//! `shep init`: scaffold a Flockfile, or add an app to one that exists.
//!
//! Design: `docs/brainstorming/specs/2026-08-18-flockfile-templates-design.md`.
//!
//! This module is being built lesson by lesson. Right now it owns exactly one
//! thing: the commented skeleton a bare `shep init` writes.

use shep_core::config;

/// The commented Flockfile a bare `shep init` writes.
///
/// Rin's call on the design's first open question: not an empty `app = []`,
/// but a file carrying a commented-out `[[app]]` showing its options, so the
/// scaffolded file is the reference documentation. A `[dog.<name>]` block
/// joins it in a later lesson.
///
/// # The comment convention this file uses
///
/// Two kinds of `#` line, and the difference is load-bearing because
/// [`tests::the_skeleton_uncomments_into_a_working_flockfile`] relies on it:
///
/// - **Prose**, explaining things to a reader: `#` followed by a SPACE.
///   `# Every app the flock runs gets one of these.`
/// - **Commented-out config**, meant to be uncommented and used: `#`
///   followed immediately by the config, no space.
///   `#name = "api"`
///
/// So uncommenting the file means stripping a leading `#` from exactly the
/// lines where the next character is not a space. That rule is what lets a
/// test prove the reference is real rather than plausible-looking prose.
/// How much of the Flockfile grammar a scaffold shows.
///
/// Rin's second axis, added 2026-08-19: verbosity belongs to the moment, not
/// to the template. A newcomer and a veteran want the SAME template at
/// different depths, and folding that into `--template` would give `node` and
/// `node-full` and `python` and `python-full`.
///
/// Only [`Depth::All`] is machine-checkable. The drift test below compares it
/// against the schemars-generated schema, which works precisely because that
/// level is meant to be exhaustive. [`Depth::Curated`] is editorial judgement
/// about what matters on day one, and no test can tell anyone it has gone
/// stale -- so the friendly level is the expensive one to maintain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Depth {
    /// The fields someone needs on their first day. A file to read.
    Curated,
    /// Every option the grammar has, for someone who knows what they want
    /// and cannot remember what it is called.
    All,
}

/// The preamble a scaffolded Flockfile opens with, above the first entry.
///
/// Hand-written and staying that way: it is not per-field content, so there
/// is nothing for the generator to derive it from.
const PREAMBLE: &str = "# Manage your app in a Flockfile\n\
# Add as many apps as you would like using TOML syntax\n\n";

/// The fields [`Depth::Curated`] shows, in the order it shows them.
///
/// An explicit ordered list rather than a `curated` flag scattered across
/// `AppConfig`'s attributes, because membership and ORDER are one editorial
/// decision and belong in one place a person can read at a glance. The order
/// is a narrative: what is it, what runs, keep it alive, where it runs.
///
/// Generation cannot supply this. schemars emits properties into a sorted
/// map, so a derived curated file would read `autorestart, cwd, name,
/// script` -- alphabetical, and meaningless to someone opening it first.
/// (Recovering the struct's own declaration order would mean enabling
/// serde_json's `preserve_order` workspace-wide, which would also reorder
/// every `--format json` payload shep emits: far too large a hammer.)
///
/// Every name here is asserted to exist in the schema, so renaming a field
/// fails the build rather than silently dropping a row.
const CURATED: &[&str] = &["name", "script", "autorestart", "cwd"];

/// Group order for [`Depth::All`], coarsest concern first: what it is and
/// what runs, then what it receives, then how it is kept alive, then when.
///
/// Fields carrying no `group` in their `init` metadata sort after all of
/// these. That is deliberate rather than tidy -- half of `AppConfig` is
/// currently ungrouped, so half the full scaffold is still alphabetical, and
/// leaving those at the end makes the gap visible instead of hiding it in
/// the middle.
const GROUP_ORDER: &[&str] = &["process", "inputs", "control", "cron"];

impl Depth {
    pub(crate) fn curated() -> String {
        format!("{PREAMBLE}{}", rows(CURATED.iter().copied()))
    }

    pub(crate) fn all() -> String {
        format!(
            "{PREAMBLE}{}",
            rows(grouped_order().iter().map(String::as_str))
        )
    }
}

/// Every field name, ordered by [`GROUP_ORDER`] and alphabetically within
/// each group, with the ungrouped remainder last.
fn grouped_order() -> Vec<String> {
    let schema = config::flockfile_schema_json();
    let props = schema
        .pointer("#/$defs/AppConfig/properties")
        .expect("app config properties must exist")
        .as_object()
        .expect("props must be an object");

    let rank = |name: &str| -> usize {
        let group = props[name]["init"]["group"].as_str().unwrap_or_default();
        GROUP_ORDER
            .iter()
            .position(|known| *known == group)
            .unwrap_or(GROUP_ORDER.len())
    };

    let mut names: Vec<String> = props.keys().cloned().collect();
    // `props` is already alphabetical (schemars emits a sorted map), and a
    // stable sort by rank alone therefore leaves each group alphabetical.
    names.sort_by_key(|name| rank(name));
    names
}

/// One commented `# blurb` / `#field = value` pair per name, in the order
/// given.
///
/// The single place a scaffold row is built, for both depths. Curated and
/// full differ only in WHICH fields they ask for and in what order, never in
/// how a row looks -- so a change to the format lands in both at once, and
/// the curated file cannot drift out of step with the grammar the way a
/// hand-written string could.
fn rows<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let schema = config::flockfile_schema_json();
    let props = schema
        .pointer("#/$defs/AppConfig/properties")
        .expect("app config properties must exist")
        .as_object()
        .expect("props must be an object");

    let body = names
        .map(|name| {
            let v = props
                .get(name)
                .unwrap_or_else(|| panic!("`{name}` is not a field of AppConfig"));
            row(name, v)
        })
        .collect::<Vec<String>>()
        .join("\n");

    format!("#[[app]]{body}")
}

/// One field's comment and its commented-out line.
///
/// `init.blurb` is preferred over the `///` doc because the two have
/// different readers: several of `AppConfig`'s docs cite internal types and
/// spec sections, which mean nothing to someone editing a Flockfile.
fn row(name: &str, v: &serde_json::Value) -> String {
    let mut desc_row = String::new();
    if let Some(init_vals) = v["init"].as_object()
        && let Some(blurb) = init_vals.get("blurb")
    {
        desc_row = blurb
            .as_str()
            .map(|s| format!("\n# {s}"))
            .unwrap_or_default();
    }

    if desc_row.is_empty() {
        desc_row = v["description"]
            .as_str()
            .expect("description must be present")
            .split('\n')
            .map(|line| {
                if line.is_empty() {
                    "\n".to_string()
                } else {
                    format!("\n# {line}")
                }
            })
            .collect::<String>();
    }

    // A field's schema `default` is only a usable placeholder when it is both
    // present and non-empty: `Option<T>` fields with no value serialize their
    // `None` as `null`, but a *required* `String` field (`name`, `script`)
    // still gets a `default` key from `#[serde(default)]` at the struct
    // level, holding `String::new()`. That empty string is not a value anyone
    // would want uncommented into a Flockfile, so it is treated the same as
    // no default at all: both fall through to `init.example`.
    let has_no_real_default = v["default"].is_null() || v["default"].as_str() == Some("");
    let value = if has_no_real_default {
        v["init"]
            .as_object()
            .and_then(|init_vals| init_vals.get("example"))
            .map(|val| toml::Value::try_from(val).expect("must convert"))
            .unwrap_or(toml::Value::String(String::new()))
    } else {
        toml::Value::try_from(&v["default"]).expect("must convert")
    };

    format!("{desc_row}\n#{name} = {value}")
}

#[allow(dead_code)]
pub(crate) fn skeleton(depth: Depth) -> String {
    match depth {
        Depth::All => Depth::all(),
        Depth::Curated => Depth::curated(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::config::{FlockFormat, Flockfile, FlockfileError};

    const VARIANTS: [Depth; 2] = [Depth::Curated, Depth::All];

    /// Uncomments the skeleton per the convention on [`skeleton`]: a leading
    /// `#` is stripped only where the character after it is not a space, so
    /// prose stays commented and config becomes live.
    fn uncomment(source: &str) -> String {
        source
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                match trimmed.strip_prefix('#') {
                    Some(rest) if !rest.starts_with(' ') => rest.to_string(),
                    _ => line.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole point of a commented reference: uncomment it and it must
    /// actually work.
    ///
    /// A scaffolded file is the most-read documentation this project has,
    /// because it is read at the moment someone is deciding whether to keep
    /// using shep. A reference that looks right and does not parse is worse
    /// than no reference at all, because it is trusted.
    ///
    /// This parses through the REAL parser, not a hand-rolled check, so the
    /// skeleton cannot drift into a shape the daemon would reject.
    #[test]
    fn the_skeleton_uncomments_into_a_working_flockfile() {
        for depth in VARIANTS {
            let live = uncomment(&skeleton(depth));

            let parsed = Flockfile::parse(&live, FlockFormat::Toml).unwrap_or_else(|err| {
            panic!("the uncommented skeleton must parse as a Flockfile: {err}\n\n--- what was parsed ---\n{live}")
        });

            assert_eq!(
                parsed.apps.len(),
                1,
                "the skeleton declares exactly one example app:\n{live}"
            );

            let app = &parsed.apps[0];
            assert!(
                !app.name.is_empty(),
                "the example app needs a name, which is one of the two fields \
             normalize() actually requires"
            );
            assert!(
                !app.script.is_empty(),
                "the example app needs a script, the other required field"
            );

            // Parsing is not enough, and this test only checked that until
            // 2026-08-19. The design says a scaffold must "parse AND
            // normalize cleanly, so a template can never ship in a shape the
            // daemon would reject" -- and the gap between the two is exactly
            // where a placeholder hides. `cron_timezone = ""` is valid TOML
            // and not a timezone; `cwd = ""` is valid TOML and not a
            // directory. Absence is how TOML says "unset", so a field with
            // no default has no value to show: it needs an EXAMPLE, or no
            // line at all.
            shep_core::config::normalize_all(parsed.apps).unwrap_or_else(|err| {
                panic!("the uncommented {depth:?} scaffold must also normalize, or it teaches config the daemon would refuse: {err}")
            });
        }
    }

    /// The skeleton is a file a human reads first. It must not arrive as a
    /// wall of live config: with the comments left alone, it declares
    /// nothing at all.
    ///
    /// This is the other half of the convention. If prose and config were
    /// not distinguishable, one of these two tests would always fail.
    ///
    /// It asserts a REFUSAL, and the first version of this test did not --
    /// it expected an `Ok` carrying zero apps, which is a shape
    /// [`Flockfile::parse`] deliberately does not produce. Its own `# Errors`
    /// section says so: [`FlockfileError::NoApps`] is "parsed fine but
    /// declared no apps". Rin found it by having to comment the check out of
    /// `shep-core` to get the test green, and stopped to ask instead.
    ///
    /// Asserting the error is the better test anyway. "Declares no apps" is
    /// then the parser's own verdict rather than this test's assumption
    /// about what the parser would say.
    #[test]
    fn the_skeleton_as_written_declares_no_apps() {
        for depth in VARIANTS {
            let err = Flockfile::parse(&skeleton(depth), FlockFormat::Toml).expect_err(
                "shep init must not drop a live app into a fresh Flockfile; \
             everything starts commented out, so the parser must find none",
            );

            assert_eq!(
                err,
                FlockfileError::NoApps,
                "the skeleton must be valid TOML that simply declares nothing, \
             not TOML that fails to parse"
            );
        }
    }

    /// No em dashes or en dashes in anything a user reads. The skeleton is
    /// about as user-facing as a file gets: it is written into their repo.
    #[test]
    fn the_skeleton_carries_no_em_dashes() {
        for depth in VARIANTS {
            let text = skeleton(depth);
            assert!(!text.contains('\u{2014}'), "em dash in the skeleton");
            assert!(!text.contains('\u{2013}'), "en dash in the skeleton");
        }
    }

    /// The property that makes `--all` worth having: it cannot go stale.
    ///
    /// `crates/shep-core/assets/flockfile.schema.json` is generated from the
    /// parser's own document type via schemars, and shep-core has its own
    /// test keeping the checked-in copy honest against `AppConfig`. So a
    /// field added to `AppConfig` reaches that file automatically, and this
    /// test then fails until the `--all` scaffold mentions it too.
    ///
    /// That is the whole argument for the depth axis: the exhaustive level is
    /// the CHEAP one, because a machine maintains it. The curated level below
    /// is the expensive one, and nothing here can help with it.
    #[test]
    fn the_all_depth_names_every_option_the_schema_knows() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../shep-core/assets/flockfile.schema.json"
        ));
        let schema: serde_json::Value =
            serde_json::from_str(raw).expect("the schema is valid JSON");
        let properties = schema["$defs"]["AppConfig"]["properties"]
            .as_object()
            .expect("AppConfig declares properties in the schema");

        let text = skeleton(Depth::All);
        let missing: Vec<&String> = properties
            .keys()
            .filter(|field| !text.contains(field.as_str()))
            .collect();

        assert!(
            missing.is_empty(),
            "--all must name every option the grammar has; {} missing: {missing:?}",
            missing.len()
        );
    }

    /// Every name in [`CURATED`] must be a real field.
    ///
    /// The subset property this replaces is now true by construction --
    /// both depths draw from the same schema through the same builder, so
    /// curated cannot contain a field the full scaffold lacks. What CAN
    /// still break is a rename: `AppConfig::cwd` becoming
    /// `AppConfig::working_dir` would leave `CURATED` naming a field that no
    /// longer exists, and `rows` would panic at generation time instead of
    /// here.
    #[test]
    fn every_curated_field_is_a_real_field() {
        let schema = shep_core::config::flockfile_schema_json();
        let value: serde_json::Value = serde_json::to_value(&schema).expect("schema serializes");
        let properties = value["$defs"]["AppConfig"]["properties"]
            .as_object()
            .expect("AppConfig declares properties in the schema");

        let unknown: Vec<&&str> = CURATED
            .iter()
            .filter(|name| !properties.contains_key(**name))
            .collect();

        assert!(
            unknown.is_empty(),
            "CURATED names {} field(s) AppConfig does not have: {unknown:?}",
            unknown.len()
        );
        assert!(
            CURATED.len() < properties.len(),
            "the curated scaffold must be smaller than the full one, or the \
             depth axis is not earning its flag"
        );
    }

    /// The forcing function the `init.example` attribute exists for: a field
    /// with no real default (an `Option<T>` left `None`) has no value the
    /// schema can offer on its own, so [`Depth::all`] falls back to an empty
    /// string when it has nothing else to show. `""` is valid TOML but not a
    /// valid `ProbeConfig`, IANA timezone, or most anything else -- so that
    /// fallback is exactly the shape of bug
    /// [`the_skeleton_uncomments_into_a_working_flockfile`] exists to catch,
    /// just discovered late and by a normalize error instead of by name.
    ///
    /// This test catches it at the source instead: adding a field to
    /// `AppConfig` with a null default now fails the build immediately, and
    /// names the field, until someone decides what its scaffold line should
    /// say.
    #[test]
    fn every_null_default_field_has_an_init_example() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../shep-core/assets/flockfile.schema.json"
        ));
        let schema: serde_json::Value =
            serde_json::from_str(raw).expect("the schema is valid JSON");
        let properties = schema["$defs"]["AppConfig"]["properties"]
            .as_object()
            .expect("AppConfig declares properties in the schema");

        let missing: Vec<&String> = properties
            .iter()
            .filter(|(_, v)| v["default"].is_null() && v["init"]["example"].is_null())
            .map(|(field, _)| field)
            .collect();

        assert!(
            missing.is_empty(),
            "every AppConfig field with a null default needs `init.example`, or \
             `--all` shows it as an empty string that fails to normalize; \
             missing on: {missing:?}"
        );
    }
}
