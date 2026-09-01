// SPDX-License-Identifier: Apache-2.0
//! `vyges-ifp` CLI — floorplan initialization over a `.odb`.
//!
//! The arithmetic lives in the library and never touches the database; this binary reads the
//! tech and site grid, asks for a plan, and applies it. The split is deliberate: every rule in
//! the planner is testable without an `.odb`, and a refused plan mutates nothing.
//!
//! Exit status is the verdict: 0 applied, 1 the design cannot be floorplanned as asked,
//! 2 usage/read/write error.

use std::process::ExitCode;
use vyges_ifp::{plan, Instance, OrientedSite, Plan, PlanError, Rect, RowParity, Site};
use vyges_opendb::Db;

// ⚠️ The prefix here is the CLI GROUP this engine belongs to, and vyges-cli's MODULES
// registry is what actually decides it (`group: "physical"`). It read `vyges loom ifp`
// for a release after the construction engines were split out of the loom suite, because
// nothing ties this string to that registry -- `vyges loom ifp` is now REFUSED by the CLI,
// so the help was telling users a command that no longer runs. If the group ever moves,
// this string moves with it. Running the binary directly as `vyges-ifp` always works and
// is group-independent.
const USAGE: &str = "\
vyges physical ifp — initialize the floorplan: die area, core area, and rows

USAGE:
  vyges physical ifp run <design.odb> --die-area 'x1 y1 x2 y2' --core-area 'x1 y1 x2 y2' --site NAME
  vyges physical ifp run <design.odb> --utilization PCT --core-space 'b t l r' --site NAME
  vyges physical ifp make-rows <design.odb> --core-area 'x1 y1 x2 y2' --site NAME
  vyges physical ifp make-tracks <design.odb> [--track LAYER:xoff,xpitch,yoff,ypitch]... [--out-odb FILE]
  vyges physical ifp --describe
  vyges physical ifp --help

MAKE-ROWS:
  Rows on a die that is ALREADY set: same options as run minus the die, with the core given
  either explicitly (--core-area) or as margins off the die (--core-space).

MAKE-TRACKS:
  Routing tracks over the die, from the technology's own pitches. With no --track, every ROUTING
  layer with a non-zero routing level is taken from the LEF; --track gives one layer explicitly,
  in MICRONS, which is the form a technology's .tracks file uses. Repeatable.

OPTIONS:
  --die-area 'x1 y1 x2 y2'   die rectangle, in MICRONS
  --utilization PCT          derive the die from the placed cell area instead of giving it
  --aspect-ratio R           height/width for the derived core (default 1.0)
  --core-space 'b t l r'     margins in MICRONS, or ONE value for all four; required with
                             --utilization and refused with --die-area
  --core-area 'x1 y1 x2 y2'  core rectangle, in MICRONS
  --site NAME                the base site whose height sets the row pitch
  --additional-sites A,B     also tile rows for these sites (hybrid rows)
  --row-parity NONE|ODD|EVEN trim the row count to a parity (default NONE)
  --flip-sites A,B           shift the row-orientation phase for these sites
  --gap MICRONS              margin around a voltage domain (default: 6 x the site height)
  --out-odb FILE             write the database here (default: IN PLACE, over the input)
  --dry-run                  plan and report, write nothing
  -o FILE                    write the report to FILE instead of stdout
  --json                     emit JSON (the default)
  --describe                 print a machine-readable JSON description of the command

EXIT STATUS:
  0  applied     the floorplan was built and written
  1  refused     the design cannot be floorplanned as asked (empty die, core outside the
                 die, degenerate or mismatched site, or no row fits)
  2  error       usage error, unreadable database, no DBU scale, or a failed write
";


/// The pin, inherited from the crate every engine already depends on.
const CRATE_PIN: &str = vyges_opendb::OPENROAD_PIN;

/// The pin this binary was built against, injected into the descriptor at print time.
///
/// 🔑 **One definition for the whole programme, inherited rather than typed.** The SHA lives in
/// `openroad-pin.yaml` in `vyges-opendb-lib` and reaches here through `vyges-opendb`, which this
/// engine already depends on. Before this, every engine spelled the pin out in its own
/// `--describe` prose, and four of them were still quoting the previous one a day after it moved.
///
/// ⚠️ **It reports what this BINARY was built against — not that the binary is current.** A stale
/// build reports its stale pin quite happily. That is the point: a harness compares this against
/// the oracle image it is about to launch and refuses on a mismatch, which is the check that was
/// missing when two engines ran a whole gate against the previous pin's oracle.
const PIN_TOKEN: &str = "@OPENROAD_PIN@";

fn describe() -> String {
    DESCRIBE.replace(PIN_TOKEN, CRATE_PIN)
}

const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "openroad_pin": "@OPENROAD_PIN@",
  "name": "ifp",
  "summary": "floorplan initialization: die area, site-grid snapping, rows, and the core area they cover",
  "maturity": "structured",
  "provenance_limitations": [
      "input_hash covers the argument vector, not the content of the .odb it names.",
      "Implements BOTH forms of initialize_floorplan. Give --die-area and --core-area explicitly, or give --utilization with --core-space and the die is derived from the placed cell area. The two are mutually exclusive, as upstream has them: --die-area with --utilization is refused (IFP-14), and so is --core-area (IFP-20).",
      "The utilization form is TWO steps and the intermediate matters. The die is derived first -- core_width from sqrt(design area / utilization / aspect ratio) TRUNCATED to a whole DBU, core_height ROUNDED from that already-truncated width -- and then snapped to the manufacturing grid. The core is taken back off the SNAPPED die by subtracting the same margins, so it is not the rectangle the die computation laid out. Both are upstream behaviours.",
      "Areas are given in MICRONS, matching the upstream Tcl argument, and converted with the database's dbu_per_micron. A database with no DBU scale is an error rather than an assumed scale.",
      "The core's lower left is snapped UP to the site grid while the upper right is left where it was; the core area finally stored is what the rows COVER, not what was asked for. Both are upstream behaviors and both are load-bearing -- a caller that reads back the core area will not always get its own argument.",
      "Rows are named globally across sites (ROW_0, ROW_1, ...) rather than restarting per site, so adding a site renumbers the rows that follow it.",
      "Existing rows are cleared before the new ones are built. Anything already placed on the old row grid is not re-legalized by this engine.",
      "Written against the upstream ifp regression goldens at pin @OPENROAD_PIN@. The algorithm is reimplemented from the published behavior, not transliterated; where the two disagree the goldens are the arbiter.",
      "MEASURED against that suite at the same pinned commit, re-run 2026-09-01, on THREE axes. Log lines: 23 cases reproduce every compared IFP-* line exactly, 0 fail, 17 not comparable (6 utilization form, 6 polygon floorplans, 3 that never call initialize_floorplan, 2 that need UPF). Track patterns: 8 of the 8 cases that call make_tracks match the reference database exactly, none skipped. Rows and die area, against the DEF goldens upstream ships: 21 comparable, of which 5 differed until the row cutting and the voltage-domain split landed. The log-line number alone was green throughout all five, which is why it is quoted last.",
      "HYBRID SITES are supported: a site with a row pattern tiles the core from that pattern (IFP-0049) and every hybrid site additionally gets rows spanning a whole pattern each (IFP-0050), offset to where its pattern occurs in the base pattern -- matching as written (R0) or reversed with orientations mirrored (MX). Row parity is REFUSED on a hybrid floorplan (IFP-0051), because parity would have to trim whole patterns rather than rows.",
      "Sites are visited in NAME order and deduplicated by name, not in the order given on the command line -- row numbering and log order both follow from this. The site set also includes sites used by placed instances that were never named as arguments (upstream addUsedSites), excluding blocks.",
      "VOLTAGE AND POWER DOMAINS split the rows. A row crossing a domain group's region, or lying within a margin of it, is replaced by up to three pieces: one left of the domain, one right of it, and -- only where the row lies wholly inside the domain's y range -- one across the domain itself. The margin is --gap, or 6x the minimum site height when none is given. Rows on PAD sites are never touched. The split happens AFTER the core area and the per-site row counts are settled, so IFP-0001 and IFP-0102 report the floorplan before it.",
      "ROWS ARE CUT against the block's placement blockages, using OpenDB's own cutRows rather than a reimplementation of it. This runs last and unconditionally; a design that declares no blockage is unaffected.",
      "SCOPE -- upstream ifp exposes FOUR commands and this engine implements THREE: initialize_floorplan is `run`, make_rows is `make-rows`, make_tracks is `make-tracks`. insert_tiecells is NOT implemented.",
      "make-rows builds rows on a die the database already holds and never writes a die of its own. The core is given explicitly or as margins off that die, and an empty die is refused with IFP-63 or IFP-64 depending on which of the two forms was used -- upstream uses two codes for the one condition.",
      "Known gap -- UPF POWER DOMAINS. Upstream's floorplan inserts power-domain instances and its instance census rises accordingly (16 to 40 on upf_test); this engine inserts none. All floorplan GEOMETRY matches exactly on those cases; what differs is the instance census that follows from the count -- IFP-0103, IFP-0104 and IFP-0105 together.",
      "A macro larger than the core area is refused with IFP-0002 before anything is snapped or written, matching upstream's ordering: the die checks come first, so a design with both an empty die and an oversized macro reports the die. Pads and covers are exempt, and a master with R90 symmetry is measured against the core's larger dimension because it is free to rotate.",
      "The instance census (IFP-0103 total instance area, IFP-0104 effective utilization) counts EVERY instance's master area, including the pads and covers the fit check skips -- it is a census of the design, not a question about the core. Utilization is omitted rather than printed as infinity when the core area is zero.",
      "make-tracks covers BOTH pitch forms. A layer whose technology carries LEF58_PITCH (FIRSTLASTPITCH) is not one grid at the layer pitch: it expands into a stack of patterns, one per track within the cell row, each repeating on the CORE ROW HEIGHT, with one further pattern past the end. A layer whose x or y offset runs past the die is skipped ENTIRELY -- both axes, not just the one that overran.",
      "The default output is IN PLACE, over the input database. Pass --out-odb to write elsewhere, or --dry-run to plan without writing."
  ],
  "invocation": {
    "args_template": ["run", "{odb}"],
    "optional": [
      { "arg": "out", "flag": "-o" },
      { "arg": "out_odb", "flag": "--out-odb" }
    ],
    "emits_json": true
  },
  "inputs": {
    "type": "object",
    "required": ["odb", "die_area", "core_area", "site"],
    "properties": {
      "odb": { "type": "string", "description": "path to the design database (.odb)" },
      "die_area": { "type": "string", "description": "die rectangle in microns, 'x1 y1 x2 y2'" },
      "core_area": { "type": "string", "description": "core rectangle in microns, 'x1 y1 x2 y2'" },
      "site": { "type": "string", "description": "base site name" },
      "out_odb": { "type": "string", "description": "write the database here instead of in place" },
      "out": { "type": "string", "description": "write the report to FILE instead of stdout" }
    }
  },
  "consumes": ["odb"],
  "produces": ["odb"],
  "artifacts": [ { "role": "floorplan_report", "field": "report_path" } ],
  "assertion": {
    "id": "floorplan-built",
    "field": "status",
    "pass_when": { "eq": "applied" }
  }
}
"#;

/// Parsed command line. Areas are held in microns until a database supplies the scale.
#[derive(Debug)]
struct Cli {
    odb: String,
    die: [f64; 4],
    core: [f64; 4],
    site: String,
    additional: Vec<String>,
    parity: RowParity,
    flipped: Vec<String>,
    out_odb: Option<String>,
    report: Option<String>,
    dry_run: bool,
    /// `-gap` in MICRONS. `None` is upstream's `INT32_MIN` sentinel for "not given", which makes
    /// the voltage-domain margin **6 x the minimum site height** instead.
    gap_um: Option<f64>,
    /// `-utilization` as a PERCENTAGE. When present the die is derived rather than given, and
    /// `--die-area`/`--core-area` are refused (upstream IFP-14 and IFP-20).
    utilization: Option<f64>,
    /// `-aspect_ratio`; upstream's default is **1.0**, not the core's shape.
    aspect_ratio: Option<f64>,
    /// `-core_space` in MICRONS, as (bottom, top, left, right). One value on the command line
    /// fills all four, which is upstream's own shorthand.
    core_space: Option<[f64; 4]>,
    /// The die and core outlines as given, in MICRONS. **More than four coordinates means a
    /// POLYGON floorplan** — upstream tests exactly that and switches the whole command over.
    die_pts: Vec<f64>,
    core_pts: Vec<f64>,
    /// `make_rows` rather than `initialize_floorplan`: the die is READ from the database instead
    /// of written, and everything else about row building is the same.
    make_rows: bool,
}

/// `x1 y1 x2 y2`, whitespace- or comma-separated.
/// Every coordinate in an area argument, in order. Four is a rectangle; more is a POLYGON, and
/// upstream decides which form the whole command takes on exactly that count.
fn parse_coords(s: &str) -> Option<Vec<f64>> {
    s.split([' ', ',', '\t'])
        .filter(|t| !t.is_empty())
        .map(|t| t.parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()
}

fn parse_rect(s: &str) -> Option<[f64; 4]> {
    let n: Vec<f64> = s
        .split([' ', ',', '\t'])
        .filter(|t| !t.is_empty())
        .map(|t| t.parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()?;
    <[f64; 4]>::try_from(n).ok()
}

fn parse_list(s: &str) -> Vec<String> {
    s.split([' ', ','])
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Microns to DBU, rounded — the same conversion upstream's `micronsToDbu` makes.
fn to_dbu(microns: f64, dbu: f64) -> i32 {
    (microns * dbu).round() as i32
}

/// `make_rows` is `initialize_floorplan` with the die left alone: same row parameters, same
/// `-core_area` / `-core_space` choice, and no die argument at all.
fn parse_args_make_rows(args: &[String]) -> Result<Cli, String> {
    let mut cli = parse_args_inner(args, true)?;
    cli.make_rows = true;
    Ok(cli)
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    parse_args_inner(args, false)
}

fn parse_args_inner(args: &[String], make_rows: bool) -> Result<Cli, String> {
    let mut odb: Option<String> = None;
    let (mut die, mut core, mut site) = (None, None, None);
    let mut cli = Cli {
        odb: String::new(),
        die: [0.0; 4],
        core: [0.0; 4],
        site: String::new(),
        additional: Vec::new(),
        parity: RowParity::None,
        flipped: Vec::new(),
        out_odb: None,
        report: None,
        dry_run: false,
        gap_um: None,
        die_pts: Vec::new(),
        core_pts: Vec::new(),
        make_rows: false,
        utilization: None,
        aspect_ratio: None,
        core_space: None,
    };

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        // Every option below takes a value; take it once here so a missing one is one message.
        let mut value = || -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{a} needs a value"))
        };
        match a {
            "--die-area" => {
                let v = value()?;
                cli.die_pts = parse_coords(&v)
                    .ok_or_else(|| format!("--die-area wants numbers, got `{v}`"))?;
                die = parse_rect(&v);
                if die.is_none() && cli.die_pts.len() <= 4 {
                    return Err(format!("--die-area wants 'x1 y1 x2 y2', got `{v}`"));
                }
            }
            "--core-area" => {
                let v = value()?;
                cli.core_pts = parse_coords(&v)
                    .ok_or_else(|| format!("--core-area wants numbers, got `{v}`"))?;
                core = parse_rect(&v);
                if core.is_none() && cli.core_pts.len() <= 4 {
                    return Err(format!("--core-area wants 'x1 y1 x2 y2', got `{v}`"));
                }
            }
            "--site" => site = Some(value()?),
            "--additional-sites" => cli.additional = parse_list(&value()?),
            "--flip-sites" => cli.flipped = parse_list(&value()?),
            "--row-parity" => {
                let v = value()?;
                cli.parity = RowParity::parse(&v)
                    .ok_or_else(|| format!("--row-parity wants NONE, ODD or EVEN, got `{v}`"))?;
            }
            "--utilization" => {
                let v = value()?;
                cli.utilization = Some(v.trim().parse::<f64>()
                    .map_err(|_| format!("--utilization wants a percentage, got `{v}`"))?);
            }
            "--aspect-ratio" => {
                let v = value()?;
                cli.aspect_ratio = Some(v.trim().parse::<f64>()
                    .map_err(|_| format!("--aspect-ratio wants a number, got `{v}`"))?);
            }
            "--core-space" => {
                // ⚠️ ONE value fills all four sides; FOUR are BOTTOM TOP LEFT RIGHT, which is
                // upstream's order and not the (left, bottom, right, top) a rectangle suggests.
                let v = value()?;
                let f: Vec<f64> = v
                    .split_whitespace()
                    .map(|t| t.parse::<f64>().map_err(|_| format!("--core-space: not a number: `{t}`")))
                    .collect::<Result<_, _>>()?;
                cli.core_space = Some(match f.len() {
                    1 => [f[0], f[0], f[0], f[0]],
                    4 => [f[0], f[1], f[2], f[3]],
                    // Upstream IFP-13, with its own words.
                    _ => return Err(
                        "IFP-0013 -core_space is either a list of 4 margins or one value for all \
                         margins.".to_string()),
                });
            }
            "--gap" => {
                // ⚠️ MICRONS here, converted once the database's scale is known -- the same
                // shape as --die-area. Upstream's Tcl converts with `ord::microns_to_dbu` and
                // leaves a sentinel when the option is absent.
                let v = value()?;
                cli.gap_um = Some(
                    v.trim()
                        .parse::<f64>()
                        .map_err(|_| format!("--gap wants a number in microns, got `{v}`"))?,
                );
            }
            "--out-odb" => cli.out_odb = Some(value()?),
            "-o" => cli.report = Some(value()?),
            "--dry-run" => cli.dry_run = true,
            "--json" => {} // the only format today; accepted so callers can be explicit
            a if a.starts_with('-') => return Err(format!("unknown option `{a}`")),
            a => odb = Some(a.to_string()),
        }
        i += 1;
    }

    cli.odb = odb.ok_or("needs a path to a .odb")?;
    // ⛔ IFP-35 is upstream's own message, and `-site` is required by BOTH commands: it is
    // `parse_row_params` that demands it, and both helpers call that first.
    cli.site = site.ok_or("IFP-0035 use -site to add placement rows.")?;

    if make_rows {
        // ⛔ `make_rows_helper` takes `-core_area` if present, else `-core_space`, else IFP-62.
        // There is no die argument: the die is whatever the database already holds.
        if die.is_some() || !cli.die_pts.is_empty() {
            return Err("make-rows does not take a die area; it uses the one already set"
                       .to_string());
        }
        if let Some(c) = core {
            cli.core = c;
        } else if cli.core_space.is_none() {
            return Err("IFP-0062 no -core_area or -core_space specified.".to_string());
        }
        return Ok(cli);
    }


    // ⛔ Upstream's own exclusions, with its own message ids. `-utilization` derives the die, so
    // an explicit die or core is a contradiction rather than an override.
    if cli.utilization.is_some() {
        if die.is_some() {
            return Err("IFP-0014 -die_area cannot be used with -utilization.".to_string());
        }
        if core.is_some() {
            return Err("IFP-0020 -core_area cannot be used with -utilization.".to_string());
        }
        // IFP-34: the spacings are what place the core inside the derived die, so there is no
        // default for them.
        if cli.core_space.is_none() {
            return Err("IFP-0034 no -core_space specified.".to_string());
        }
    } else {
        if cli.aspect_ratio.is_some() {
            return Err("IFP-0033 -aspect_ratio cannot be used with -die_area.".to_string());
        }
        if cli.core_space.is_some() && die.is_some() {
            return Err("IFP-0024 -core_space cannot be used with -die_area.".to_string());
        }
        // ⛔ POLYGON MODE: upstream switches on the COORDINATE COUNT of either area, and then
        // both must be polygons — `make_polygon_die_helper` requires `-die_area` (IFP-75) and
        // `make_polygon_rows_helper` requires `-core_area` (IFP-85).
        // ⛔ POLYGON MODE is decided here and VALIDATED IN `run_polygon`, not here. Upstream
        // prints IFP-5 and IFP-106 before it ever looks at `-core_area`, so validating both
        // outlines up front reports the right error at the wrong point in the sequence — which is
        // exactly what `init_floorplan_polygon2`'s golden catches.
        if cli.die_pts.len() > 4 || cli.core_pts.len() > 4 {
        } else {
            cli.die = die.ok_or("`run` needs --die-area or --utilization")?;
            cli.core = core.ok_or("`run` needs --core-area")?;
        }
    }
    Ok(cli)
}

/// Report the plan the way the goldens do (R11), then a census line.
///
/// The upstream message ids are quoted in the text because that is what makes a log diff
/// against `initialize_floorplan` mean something; the code field stays in the house
/// `<ENGINE>-<KIND>` form so the trail is queryable.
fn emit_events(p: &Plan, dbu: f64, applied: bool, num_insts: usize, instances: &[Instance]) {
    use vyges_events::{Event, Severity};
    let um = |v: i32| (v as f64) / dbu;

    if p.core_was_snapped() {
        vyges_events::emit(
            &Event::new(
                "vyges-ifp",
                Severity::Warn,
                format!(
                    "IFP-0028 Core area lower left ({:.3}, {:.3}) snapped to ({:.3}, {:.3}).",
                    um(p.core_requested.x_min),
                    um(p.core_requested.y_min),
                    um(p.core_snapped.x_min),
                    um(p.core_snapped.y_min),
                ),
            )
            .with_code("IFP-CORE-SNAP"),
        );
    }

    // A hybrid floorplan reports its pattern rows first, then a line per hybrid site; a uniform
    // one reports a single line per site. The two constructions are genuinely different, so they
    // do not share a message.
    if let Some((base, n)) = &p.pattern_rows {
        vyges_events::emit(
            &Event::new(
                "vyges-ifp",
                Severity::Info,
                format!("IFP-0049 Added {n} rows from site {base} row pattern."),
            )
            .with_code("IFP-PATTERN-ROWS")
            .with_objects(vec![format!("site:{base}")]),
        );
    }
    let rows_x = p.rows.first().map(|r| r.num_sites).unwrap_or(0);
    for (site, count) in &p.rows_per_site {
        if *count == 0 {
            // A site that tiled nothing is said out loud; silence would read as "it worked".
            vyges_events::emit(
                &Event::new(
                    "vyges-ifp",
                    Severity::Warn,
                    format!("IFP-0061 No rows created for site {site}."),
                )
                .with_code("IFP-NO-ROWS-SITE")
                .with_objects(vec![format!("site:{site}")]),
            );
        } else {
            vyges_events::emit(
                &Event::new(
                    "vyges-ifp",
                    Severity::Info,
                    if p.pattern_rows.is_some() {
                        format!("IFP-0050 Added {count} rows of site {site}.")
                    } else {
                        format!("IFP-0001 Added {count} rows of {rows_x} site {site}.")
                    },
                )
                .with_code(if p.pattern_rows.is_some() {
                    "IFP-HYBRID-ROWS"
                } else {
                    "IFP-ROWS"
                })
                .with_objects(vec![format!("site:{site}")]),
            );
        }
    }

    // The census upstream prints after a floorplan, reproduced so a log diff against
    // `initialize_floorplan` compares like with like.
    let design_um2 = vyges_ifp::design_area(instances) / (dbu * dbu);
    let core_um2 = um(p.core_final.dx()) * um(p.core_final.dy());
    let mut census = vec![
        (
            "IFP-CENSUS-DIE",
            format!(
                "IFP-0100 Die BBox: ( {:.3} {:.3} ) ( {:.3} {:.3} ) um",
                um(p.die.x_min),
                um(p.die.y_min),
                um(p.die.x_max),
                um(p.die.y_max)
            ),
        ),
        (
            "IFP-CENSUS-CORE",
            format!(
                "IFP-0101 Core BBox: ( {:.3} {:.3} ) ( {:.3} {:.3} ) um",
                um(p.core_final.x_min),
                um(p.core_final.y_min),
                um(p.core_final.x_max),
                um(p.core_final.y_max)
            ),
        ),
        (
            "IFP-CENSUS-AREA",
            format!("IFP-0102 Core area: {core_um2:.3} um^2"),
        ),
        (
            "IFP-CENSUS-DESIGN-AREA",
            format!("IFP-0103 Total instances area: {design_um2:.3} um^2"),
        ),
    ];
    // Upstream prints utilization only when there is a core to divide by, and so do we: a
    // "utilization" of infinity or NaN would be a worse answer than no line at all.
    if core_um2 > 0.0 {
        census.push((
            "IFP-CENSUS-UTIL",
            format!(
                "IFP-0104 Effective utilization: {:.3}",
                design_um2 / core_um2
            ),
        ));
    }
    census.push((
        "IFP-CENSUS-INSTS",
        format!("IFP-0105 Number of instances: {num_insts}"),
    ));
    for (code, text) in census {
        vyges_events::emit(&Event::new("vyges-ifp", Severity::Info, text).with_code(code));
    }

    vyges_events::emit(
        &Event::new(
            "vyges-ifp",
            Severity::Info,
            format!(
                "floorplan {}: {} row(s), core ({:.3}, {:.3}) ({:.3}, {:.3}) um",
                if applied { "applied" } else { "planned" },
                p.rows.len(),
                um(p.core_final.x_min),
                um(p.core_final.y_min),
                um(p.core_final.x_max),
                um(p.core_final.y_max),
            ),
        )
        .with_code("IFP-DONE"),
    );
}

fn rect_json(r: &Rect, dbu: f64) -> String {
    format!(
        "{{ \"dbu\": [{}, {}, {}, {}], \"um\": [{:.4}, {:.4}, {:.4}, {:.4}] }}",
        r.x_min,
        r.y_min,
        r.x_max,
        r.y_max,
        r.x_min as f64 / dbu,
        r.y_min as f64 / dbu,
        r.x_max as f64 / dbu,
        r.y_max as f64 / dbu
    )
}

/// The `loom-result` envelope. Hand-written rather than derived so the report stays a stable
/// contract even as the plan's internals move.
fn report_json(p: &Plan, dbu: f64, status: &str, odb_written: Option<&str>) -> String {
    let per_site = p
        .rows_per_site
        .iter()
        .map(|(s, c)| format!("    {{ \"site\": \"{s}\", \"rows\": {c} }}"))
        .collect::<Vec<_>>()
        .join(",\n");
    let written = match odb_written {
        Some(p) => format!("\"{p}\""),
        None => "null".to_string(),
    };
    format!(
        "{{
  \"tool\": \"vyges-ifp\",
  \"status\": \"{status}\",
  \"dbu_per_micron\": {dbu},
  \"die_area\": {die},
  \"core_area_requested\": {req},
  \"core_area_snapped\": {snap},
  \"core_area\": {fin},
  \"core_was_snapped\": {snapped},
  \"rows\": {rows},
  \"sites_per_row\": {sites_per_row},
  \"rows_per_site\": [
{per_site}
  ],
  \"odb_written\": {written}
}}",
        dbu = dbu as i64,
        die = rect_json(&p.die, dbu),
        req = rect_json(&p.core_requested, dbu),
        snap = rect_json(&p.core_snapped, dbu),
        fin = rect_json(&p.core_final, dbu),
        snapped = p.core_was_snapped(),
        rows = p.rows.len(),
        sites_per_row = p.rows.first().map(|r| r.num_sites).unwrap_or(0),
    )
}

/// Write the plan into the database: die area, rows, then the core area the rows cover.
///
/// Every voltage or power domain in the block, as `updateVoltageDomain` selects them.
///
/// ⛔ **The TYPE is the selector and there is no substitute for it.** A `PHYSICAL_CLUSTER` group
/// can carry a name and a region just as a domain does, so filtering on "has a region" would be a
/// guess that happens to hold on the designs at hand. `dbGroup::getType` was bound for this.
///
/// A group whose region has no boundaries contributes an empty box; upstream starts its bounds at
/// the integer extremes and would carry those through, so such a group is dropped here rather than
/// left to produce a nonsense rectangle.
fn voltage_domains(db: &Db) -> Vec<vyges_ifp::Domain> {
    let mut out = Vec::new();
    for group in db.block_get_groups() {
        match db.group_get_type(&group).unwrap_or_default().as_str() {
            "VOLTAGE_DOMAIN" | "POWER_DOMAIN" => {}
            _ => continue,
        }
        let region = db.group_get_region(&group);
        if region.is_empty() {
            continue;
        }
        let bounds = db.region_boundaries(&region).unwrap_or_default();
        if bounds.is_empty() {
            continue;
        }
        let bbox = vyges_ifp::Rect {
            x_min: bounds.iter().map(|b| b.0).min().unwrap(),
            y_min: bounds.iter().map(|b| b.1).min().unwrap(),
            x_max: bounds.iter().map(|b| b.2).max().unwrap(),
            y_max: bounds.iter().map(|b| b.3).max().unwrap(),
        };
        out.push(vyges_ifp::Domain { name: db.group_get_name(&group), bbox });
    }
    out
}

/// Order matters — the core area is *derived* from the rows (R9), so it cannot be set first.
///
/// ⛔ **And it is derived from the rows BEFORE any voltage-domain split.** Upstream sets it at the
/// end of `makeUniformRows`, and only then does `makeRows` call `updateVoltageDomain`; deriving it
/// from the split rows instead gives a different rectangle and moves IFP-0102 and IFP-0104. So the
/// unsplit rows are written first, the core area is taken from them by odb's own
/// `computeCoreArea`, and the split set replaces them afterwards.
fn apply(db: &mut Db, p: &Plan, split: Option<&[vyges_ifp::Row]>, set_die: bool)
    -> Result<(), String>
{
    // ⛔ `make_rows` does not write a die — it builds rows on the one already there. Writing the
    // plan's die back would look harmless (it is the same rectangle, read from this database) but
    // `plan` snaps it to the manufacturing grid first, so a die that was NOT on the grid would be
    // silently moved by a command that has no business touching it.
    if set_die {
        db.set_die_area(p.die.x_min, p.die.y_min, p.die.x_max, p.die.y_max)
            .map_err(|e| format!("cannot set the die area: {e}"))?;
    }
    db.clear_rows()
        .map_err(|e| format!("cannot clear the existing rows: {e}"))?;
    // ⛔ **Written ONCE, in the order the planner decided.** Clearing and re-creating a second
    // time does not reproduce that order: `clear_rows` frees every table slot and the next
    // creations reuse them, so a write-then-rewrite scrambles the row list. The voltage-domain
    // split is therefore computed before anything is written, not applied to written rows.
    for r in split.unwrap_or(&p.rows) {
        db.create_row(
            &r.name,
            &r.site,
            r.x,
            r.y,
            &r.orient,
            "HORIZONTAL",
            r.num_sites,
            r.spacing,
        )
        .map_err(|e| format!("cannot create {}: {e}", r.name))?;
    }
    // ⛔ **The core area comes from the plan, which derived it from the rows BEFORE the split.**
    // Upstream sets it at the end of `makeUniformRows` and splits afterwards, so asking odb to
    // recompute it from what is now in the database would give a different rectangle -- that
    // regressed IFP-0102 and IFP-0104 on two cases before this line was written. `p.core_final`
    // is the same value the engine reports as IFP-0101, which the goldens already agree with.
    db.set_core_area(
        p.core_final.x_min,
        p.core_final.y_min,
        p.core_final.x_max,
        p.core_final.y_max,
    )
    .map_err(|e| format!("cannot set the core area: {e}"))?;

    // ⛔ **LAST, and unconditional** — upstream's `makeRows` ends by handing every `dbBlockage` in
    // the block to `odb::cutRows`, outside the non-negative-core guard, and `cutRows` returns at
    // once when there are none. Both zeros are upstream's own arguments: `min_row_height` 0
    // disables odb's narrow-region removal, and the halos are 0.
    //
    // ⚠️ It runs AFTER the core area is set, which is upstream's order too — cutting rows does not
    // change the core area that was already derived from the uncut ones.
    db.cut_rows_at_blockages(0, 0, 0, 0)
        .map_err(|e| format!("cannot cut the rows around the blockages: {e}"))
}

/// A refusal is a verdict about the design, not a crash: exit 1, and nothing was written.
fn refuse(e: PlanError, dbu: f64) -> ExitCode {
    use vyges_events::{Event, Severity};
    let code = match e {
        PlanError::NoRows { .. } => "IFP-NO-ROWS",
        PlanError::CoreNotInDie => "IFP-CORE-OUTSIDE-DIE",
        PlanError::EmptyDieArea => "IFP-EMPTY-DIE",
        PlanError::InstanceDoesNotFit { .. } => "IFP-INST-TOO-BIG",
        PlanError::ParityWithHybridRows => "IFP-PARITY-HYBRID",
        PlanError::IncompatibleSite { .. } => "IFP-SITE-INCOMPATIBLE",
        _ => "IFP-BAD-SITE",
    };
    let um = |v: i32| (v as f64) / dbu;

    // ⛔ **Upstream had already printed two things by the time it failed**, and the goldens
    // assert both: `makeRows` warns IFP-0028 when the core's lower left moved to the site grid,
    // and `makeUniformRows` warns IFP-0061 for every site that produced no rows. Only then does
    // IFP-0065 abort. Emitting the error alone is a faithful message in the wrong call sequence.
    if let PlanError::NoRows { core_requested, core_snapped, ref empty_sites } = e {
        if core_snapped.x_min != core_requested.x_min || core_snapped.y_min != core_requested.y_min
        {
            vyges_events::emit(
                &Event::new(
                    "vyges-ifp",
                    Severity::Warn,
                    format!(
                        "IFP-0028 Core area lower left ({:.3}, {:.3}) snapped to ({:.3}, {:.3}).",
                        um(core_requested.x_min), um(core_requested.y_min),
                        um(core_snapped.x_min), um(core_snapped.y_min)
                    ),
                )
                .with_code("IFP-CORE-SNAPPED"),
            );
        }
        for site in empty_sites {
            vyges_events::emit(
                &Event::new(
                    "vyges-ifp",
                    Severity::Warn,
                    format!("IFP-0061 No rows created for site {site}."),
                )
                .with_code("IFP-NO-ROWS-FOR-SITE")
                .with_objects(vec![format!("site:{site}")]),
            );
        }
    }

    let text = match e {
        // The ones the goldens name, in their words.
        PlanError::NoRows { .. } => "IFP-0065 No rows created in the core area.".to_string(),
        PlanError::ParityWithHybridRows => {
            "IFP-0051 Constraining row parity is not supported for hybrid rows.".to_string()
        }
        PlanError::IncompatibleSite { ref site, ref base } => {
            format!("IFP-0048 Site {site} is incompatible with site {base}")
        }
        PlanError::InstanceDoesNotFit {
            ref name,
            width,
            height,
            core,
        } => format!(
            "IFP-0002 {name} ({:.3}um, {:.3}um) does not fit in the core area: \
             ({:.3}um, {:.3}um) - ({:.3}um, {:.3}um)",
            um(width),
            um(height),
            um(core.x_min),
            um(core.y_min),
            um(core.x_max),
            um(core.y_max)
        ),
        ref other => other.to_string(),
    };
    vyges_events::emit(&Event::new("vyges-ifp", Severity::Error, text).with_code(code));
    eprintln!("vyges-ifp: {e}");
    ExitCode::from(1)
}

/// `reportAreas` for the polygon path, read back from the database.
///
/// The rectangular path reports from the plan it just built; here the die is a polygon and the
/// core is whatever the clipped rows cover, so both come from the database rather than from
/// arithmetic. The lines and their formatting are the same — they are the same `reportAreas`.
fn report_polygon_areas(db: &Db, dbu: f64, instances: &[Instance]) {
    use vyges_events::{Event, Severity};
    let um = |v: i32| (v as f64) / dbu;
    let (dxa, dya, dxb, dyb) = (
        db.block_get_die_area_x_min(), db.block_get_die_area_y_min(),
        db.block_get_die_area_x_max(), db.block_get_die_area_y_max(),
    );
    let (cxa, cya, cxb, cyb) = (
        db.block_get_core_area_x_min(), db.block_get_core_area_y_min(),
        db.block_get_core_area_x_max(), db.block_get_core_area_y_max(),
    );
    let core_um2 = um(cxb - cxa) * um(cyb - cya);
    let design_um2 = vyges_ifp::design_area(instances) / (dbu * dbu);

    let mut census: Vec<(&str, String)> = vec![
        ("IFP-CENSUS-DIE", format!(
            "IFP-0100 Die BBox: ( {:.3} {:.3} ) ( {:.3} {:.3} ) um",
            um(dxa), um(dya), um(dxb), um(dyb))),
        ("IFP-CENSUS-CORE", format!(
            "IFP-0101 Core BBox: ( {:.3} {:.3} ) ( {:.3} {:.3} ) um",
            um(cxa), um(cya), um(cxb), um(cyb))),
        ("IFP-CENSUS-AREA", format!("IFP-0102 Core area: {core_um2:.3} um^2")),
        ("IFP-CENSUS-DESIGN-AREA",
         format!("IFP-0103 Total instances area: {design_um2:.3} um^2")),
    ];
    if core_um2 > 0.0 {
        census.push(("IFP-CENSUS-UTIL",
            format!("IFP-0104 Effective utilization: {:.3}", design_um2 / core_um2)));
    }
    census.push(("IFP-CENSUS-INSTS",
        format!("IFP-0105 Number of instances: {}", db.num_insts())));
    for (code, text) in census {
        vyges_events::emit(&Event::new("vyges-ifp", Severity::Info, text).with_code(code));
    }
}

/// The polygon form of `initialize_floorplan` — upstream's `makePolygonDie` + `makePolygonRows`.
///
/// 🔑 **The call sequence, which is what makes this a separate function rather than a flag**
/// (`InitFloorplan.cc:212` and `:266`, reached through the Tcl's `make_polygon_die_helper` and
/// `make_polygon_rows_helper`):
///
/// ```text
/// IFP-5    "Added N die polygon vertices to the list."    (the Tcl, before anything)
/// makePolygonDie:   IFP-106, >=4 vertices, snap EVERY vertex to the mfg grid, store, resetTracks
/// makePolygonRows:  >=4 core vertices, snap them too, die must contain the core BBOX (IFP-1004),
///                   checkInstanceDimensions against that BBOX, clear rows
/// makePolygonRowsScanline: refuse a hybrid base site (IFP-1000); snap the bbox lower left UP to
///                   the site grid (IFP-1003 if it moved); per site in NAME order, height-multiple
///                   check (IFP-1001) then rows clipped to the polygon (IFP-1002 each);
///                   updateVoltageDomain, then cutRows
/// IFP-997  "Completed polygon-aware row generation using N vertices"   (N = points - 1)
/// reportAreas
/// ```
///
/// ⚠️ **`points.size() - 1`**: odb closes the ring, so an 8-vertex input reads back as 9 points
/// and the line prints 8.
#[allow(clippy::too_many_arguments)]
fn run_polygon(
    db: &mut Db,
    cli: &Cli,
    _dbu: i32,
    dbu_f: f64,
    grid: Option<i32>,
    gap: Option<i32>,
    sites: &[Site],
    instances: &[Instance],
) -> ExitCode {
    use vyges_events::{Event, Severity};
    let um = |v: i32| (v as f64) / dbu_f;
    // Microns -> DBU, then every vertex snapped to the manufacturing grid, which both
    // `makePolygonDie` and `makePolygonRows` do to their own point lists.
    let to_poly = |pts: &[f64]| -> Vec<(i32, i32)> {
        pts.chunks(2)
            .map(|c| {
                (
                    vyges_ifp::snap_to_mfg_grid(to_dbu(c[0], dbu_f), grid),
                    vyges_ifp::snap_to_mfg_grid(to_dbu(c[1], dbu_f), grid),
                )
            })
            .collect()
    };
    let fail = |code: &str, text: String| -> ExitCode {
        vyges_events::emit(&Event::new("vyges-ifp", Severity::Error, text).with_code(code));
        ExitCode::from(1)
    };

    // ⛔ **Upstream's order, and it is observable.** `make_polygon_die_helper` validates the DIE
    // and prints IFP-5, `makePolygonDie` prints IFP-106 — and only THEN does
    // `make_polygon_rows_helper` look at `-core_area`. A case with a good die and no core sees
    // IFP-5 and IFP-106 before its IFP-85; a case with a malformed die sees neither.
    if cli.die_pts.is_empty() {
        return fail("IFP-NO-DIE-POLYGON",
                    "IFP-0075 no -die_area specified for polygon floorplan.".to_string());
    }
    if cli.die_pts.len() % 2 != 0 {
        return fail("IFP-DIE-POLYGON-ODD",
            "IFP-0076 -die_area must have an even number of coordinates (x y pairs).".to_string());
    }
    if cli.die_pts.len() < 8 {
        return fail("IFP-DIE-POLYGON-SHORT",
            "IFP-0077 -die_area must have at least 4 vertices (8 coordinates).".to_string());
    }

    let die_poly = to_poly(&cli.die_pts);
    vyges_events::emit(&Event::new(
        "vyges-ifp",
        Severity::Info,
        format!("IFP-0005 Added {} die polygon vertices to the list.", die_poly.len()),
    ).with_code("IFP-DIE-POLYGON-VERTICES"));
    vyges_events::emit(&Event::new(
        "vyges-ifp",
        Severity::Info,
        "IFP-0106 Initializing floorplan in polygon mode.".to_string(),
    ).with_code("IFP-POLYGON-MODE"));

    if cli.core_pts.is_empty() {
        return fail("IFP-NO-CORE-POLYGON",
                    "IFP-0085 no -core_area specified for polygonal floorplan.".to_string());
    }
    if cli.core_pts.len() % 2 != 0 {
        return fail("IFP-CORE-POLYGON-ODD",
            "IFP-0082 -core_area must have an even number of coordinates (x y pairs).".to_string());
    }
    if cli.core_pts.len() < 8 {
        return fail("IFP-CORE-POLYGON-SHORT",
            "IFP-0083 -core_area must have at least 4 vertices (8 coordinates).".to_string());
    }
    let core_poly = to_poly(&cli.core_pts);

    let die_bbox = vyges_ifp::polygon_bbox(&die_poly);
    let core_bbox = vyges_ifp::polygon_bbox(&core_poly);
    if !die_bbox.contains(&core_bbox) {
        eprintln!("vyges-ifp: IFP-1004 Die area must contain the core polygon bounding box.");
        return ExitCode::from(1);
    }
    if let Err(e) = vyges_ifp::check_instance_dimensions(instances, core_bbox) {
        return refuse(e, dbu_f);
    }

    let Some(base) = sites.first() else {
        eprintln!("vyges-ifp: no site to build rows from");
        return ExitCode::from(1);
    };
    if base.is_hybrid() {
        eprintln!("vyges-ifp: IFP-1000 Hybrid rows not yet supported with polygon-aware \
                   generation.");
        return ExitCode::from(1);
    }

    // Sites in NAME order, deduplicated — the same `std::map` ordering the rectangular path uses.
    let mut ordered: Vec<&Site> = sites.iter().collect();
    ordered.sort_by(|a, b| a.name.cmp(&b.name));
    ordered.dedup_by(|a, b| a.name == b.name);

    let mut rows: Vec<vyges_ifp::Row> = Vec::new();
    let mut per_site: Vec<(String, usize)> = Vec::new();
    let mut snapped_bbox = core_bbox;

    // ⚠️ The whole row-building step is inside upstream's non-negative guard; a core whose lower
    // left is negative produces NO rows and still reaches the blockage cutting below.
    if core_bbox.x_min >= 0 && core_bbox.y_min >= 0 {
        let clx = vyges_ifp::div_ceil(core_bbox.x_min, base.width) * base.width;
        let cly = vyges_ifp::div_ceil(core_bbox.y_min, base.height) * base.height;
        if clx != core_bbox.x_min || cly != core_bbox.y_min {
            vyges_events::emit(&Event::new(
                "vyges-ifp",
                Severity::Warn,
                format!(
                    "IFP-1003 Core polygon bounding box lower left ({:.3}, {:.3}) snapped to \
                     ({:.3}, {:.3}).",
                    um(core_bbox.x_min), um(core_bbox.y_min), um(clx), um(cly)
                ),
            ).with_code("IFP-POLYGON-BBOX-SNAPPED"));
        }
        snapped_bbox = Rect::new(clx, cly, core_bbox.x_max, core_bbox.y_max);

        for site in &ordered {
            if site.height % base.height != 0 {
                eprintln!(
                    "vyges-ifp: IFP-1001 Site {} height {:.3}um is not a multiple of site {} \
                     height {:.3}um.",
                    site.name, um(site.height), base.name, um(base.height)
                );
                return ExitCode::from(1);
            }
            let flipped = cli.flipped.iter().any(|f| f == &site.name);
            let made = vyges_ifp::polygon_rows(
                site, &core_poly, snapped_bbox, cli.parity, flipped, rows.len(),
            );
            per_site.push((site.name.clone(), made.len()));
            rows.extend(made);
        }
    }

    // Write: the polygon die, then the rows, then the core area the rows cover.
    let flat: Vec<i32> = die_poly.iter().flat_map(|p| [p.0, p.1]).collect();
    if let Err(e) = db.set_die_area_polygon(&flat) {
        eprintln!("vyges-ifp: cannot set the polygon die area: {e}");
        return ExitCode::from(2);
    }
    if let Err(e) = db.clear_rows() {
        eprintln!("vyges-ifp: cannot clear the existing rows: {e}");
        return ExitCode::from(2);
    }
    for r in &rows {
        if let Err(e) = db.create_row(&r.name, &r.site, r.x, r.y, &r.orient, "HORIZONTAL",
                                      r.num_sites, r.spacing) {
            eprintln!("vyges-ifp: cannot create {}: {e}", r.name);
            return ExitCode::from(2);
        }
    }
    for (site, n) in &per_site {
        vyges_events::emit(&Event::new(
            "vyges-ifp",
            Severity::Info,
            format!("IFP-1002 Added {n} polygon-aware rows for site {site}."),
        ).with_code("IFP-POLYGON-ROWS").with_objects(vec![format!("site:{site}")]));
    }
    if let Err(e) = db.set_core_area_from_rows() {
        eprintln!("vyges-ifp: cannot set the core area: {e}");
        return ExitCode::from(2);
    }
    let _ = gap;   // the polygon path reaches updateVoltageDomain, which is not built here yet
    if let Err(e) = db.cut_rows_at_blockages(0, 0, 0, 0) {
        eprintln!("vyges-ifp: cannot cut the rows around the blockages: {e}");
        return ExitCode::from(2);
    }

    vyges_events::emit(&Event::new(
        "vyges-ifp",
        Severity::Info,
        // ⚠️ odb closes the ring, so its point count is one more than the vertices given.
        format!("IFP-0997 Completed polygon-aware row generation using {} vertices",
                core_poly.len()),
    ).with_code("IFP-POLYGON-COMPLETE"));

    report_polygon_areas(db, dbu_f, instances);

    let dest = cli.out_odb.clone().unwrap_or_else(|| cli.odb.clone());
    if let Err(e) = db.write(&dest) {
        eprintln!("vyges-ifp: cannot write {dest}: {e}");
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

fn run(args: &[String]) -> ExitCode {
    match parse_args(args) {
        Ok(cli) => run_with(cli),
        Err(e) => {
            eprintln!("vyges-ifp: {e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `make-rows`: upstream's second command. Rows on a die that is ALREADY set.
///
/// 🔑 **The die is read, never written.** `makeRows` starts from `block_->getDieArea()` and errors
/// IFP-63 when it is empty; `makeRowsWithSpacing` does the same with IFP-64 and then derives the
/// core by insetting that die. Everything after the core is fixed is the same row building
/// `initialize_floorplan` does, which is why this shares its body rather than copying it.
fn make_rows_cmd(args: &[String]) -> ExitCode {
    match parse_args_make_rows(args) {
        Ok(cli) => run_with(cli),
        Err(e) => {
            eprintln!("vyges-ifp: {e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run_with(cli: Cli) -> ExitCode {
    let mut db = match Db::open(&cli.odb) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("vyges-ifp: cannot read {}: {e}", cli.odb);
            return ExitCode::from(2);
        }
    };
    let dbu = db.dbu_per_micron();
    if dbu <= 0 {
        eprintln!("vyges-ifp: no DBU scale; cannot convert microns");
        return ExitCode::from(2);
    }
    let dbu_f = dbu as f64;

    // The site grid is read from the database, never assumed. An unknown site name is a usage
    // error caught here rather than a zero-sized site that silently tiles nothing.
    let mut sites = Vec::new();
    for name in std::iter::once(&cli.site).chain(cli.additional.iter()) {
        let (w, h) = (db.site_get_width(name), db.site_get_height(name));
        if w <= 0 || h <= 0 {
            eprintln!(
                "vyges-ifp: site `{name}` is unknown or has no extent ({w} x {h}); \
                 the library defines: {}",
                db.site_names().map(|v| v.join(", ")).unwrap_or_default()
            );
            return ExitCode::from(2);
        }
        // A hybrid site's pattern names other sites; their dimensions are resolved here so the
        // planner stays a pure function of what it is handed.
        let row_pattern = match db.row_pattern(name) {
            Ok(p) => p
                .into_iter()
                .map(|(site, orient)| {
                    let (w, h) = (db.site_get_width(&site), db.site_get_height(&site));
                    OrientedSite {
                        site,
                        orient,
                        width: w,
                        height: h,
                    }
                })
                .collect(),
            Err(e) => {
                eprintln!("vyges-ifp: cannot read the row pattern of `{name}`: {e}");
                return ExitCode::from(2);
            }
        };
        sites.push(Site {
            name: name.clone(),
            width: w,
            height: h,
            row_pattern,
        });
    }

    // Sites the DESIGN uses, beyond the ones named on the command line: upstream's
    // `addUsedSites`. A hybrid library can place cells on a site that was never mentioned as an
    // argument, and those rows still have to be built — `hybrid_rows2` needs exactly this.
    // Blocks (macros) are excluded: they are placed, not tiled into rows.
    for name in db.inst_names() {
        let master = db.inst_master(&name);
        if !db.master_is_core_auto_placeable(&master) || db.master_is_block(&master) {
            continue;
        }
        let site = db.master_get_site(&master);
        if site.is_empty() || sites.iter().any(|s| s.name == site) {
            continue;
        }
        let (w, h) = (db.site_get_width(&site), db.site_get_height(&site));
        if w <= 0 || h <= 0 {
            continue;
        }
        let row_pattern = db
            .row_pattern(&site)
            .unwrap_or_default()
            .into_iter()
            .map(|(s, orient)| {
                let (w, h) = (db.site_get_width(&s), db.site_get_height(&s));
                OrientedSite {
                    site: s,
                    orient,
                    width: w,
                    height: h,
                }
            })
            .collect();
        sites.push(Site {
            name: site,
            width: w,
            height: h,
            row_pattern,
        });
    }

    let grid = match db.manufacturing_grid() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("vyges-ifp: cannot read the manufacturing grid: {e}");
            return ExitCode::from(2);
        }
    };

    // ⛔ **`checkGap` is the FIRST thing `initFloorplan` does**, and `makeRows` and
    // `makeRowsWithSpacing` each repeat it before touching the database. A gap is rejected whether
    // or not the design has a voltage domain to use it on — validating it only where it is
    // consumed means a design with no domains accepts a gap upstream refuses.
    //
    // ⚠️ `None` is upstream's INT32_MIN sentinel for "not given"; the margin then falls back to
    // 6 x the minimum site height rather than to a number the caller never chose.
    let gap = match cli.gap_um {
        None => None,
        Some(um) => {
            let g = vyges_ifp::microns_to_mfg_grid(um, dbu, grid.unwrap_or(0));
            if g <= 0 {
                vyges_events::emit(
                    &vyges_events::Event::new(
                        "vyges-ifp",
                        vyges_events::Severity::Error,
                        format!("IFP-0036 Gap must be positive ({g})"),
                    )
                    .with_code("IFP-BAD-GAP"),
                );
                return ExitCode::from(1);
            }
            Some(g)
        }
    };

    let rect = |v: [f64; 4]| {
        Rect::new(
            to_dbu(v[0], dbu_f),
            to_dbu(v[1], dbu_f),
            to_dbu(v[2], dbu_f),
            to_dbu(v[3], dbu_f),
        )
    };
    // Read every instance's master once: the fit check (R13) and the area census (R12) both
    // need it, and each master lookup crosses the FFI boundary.
    let instances: Vec<Instance> = db
        .inst_names()
        .into_iter()
        .map(|name| {
            let m = db.inst_master(&name);
            Instance {
                name,
                width: db.master_get_width(&m) as i32,
                height: db.master_get_height(&m) as i32,
                is_pad: db.master_is_pad(&m),
                is_cover: db.master_is_cover(&m),
                symmetry_r90: db.master_get_symmetry_r90(&m),
            }
        })
        .collect();

    // ⛔ **The `-utilization` form is TWO steps, not one**, and upstream runs them through
    // separate Tcl helpers: `make_die_helper` derives and SETS the die, then `make_rows_helper`
    // re-derives the core from that die minus the same spacings. The die is snapped to the
    // manufacturing grid in between, so the core is NOT the rectangle the die computation laid
    // out — reproducing that two-step is the whole point.
    // ⛔ **POLYGON MODE takes the whole command down a different path**, and it is NOT `makeRows`:
    // `make_polygon_die_helper` + `make_polygon_rows_helper` in the Tcl, then `makePolygonDie` and
    // `makePolygonRows` in C++, with their own message ids (IFP-1003 where the rectangular path
    // warns IFP-0028, IFP-1001 where it errors IFP-0054). Hybrid sites are refused outright.
    if !cli.die_pts.is_empty() && (cli.die_pts.len() > 4 || cli.core_pts.len() > 4) {
        return run_polygon(&mut db, &cli, dbu, dbu_f, grid, gap, &sites, &instances);
    }

    // ⛔ **`make_rows` READS the die; it never writes one.** `makeRows` errors IFP-63 on an empty
    // die and `makeRowsWithSpacing` errors IFP-64 — two codes for the same condition, chosen by
    // which form the caller used, so the code carries which helper was reached.
    let (die_in, core_in) = if cli.make_rows {
        let die = Rect::new(
            db.block_get_die_area_x_min(), db.block_get_die_area_y_min(),
            db.block_get_die_area_x_max(), db.block_get_die_area_y_max(),
        );
        if die.is_empty() {
            let code = if cli.core_space.is_some() { "0064" } else { "0063" };
            vyges_events::emit(&vyges_events::Event::new(
                "vyges-ifp",
                vyges_events::Severity::Error,
                format!("IFP-{code} Floorplan die area is 0. Cannot build rows."),
            ).with_code("IFP-EMPTY-DIE"));
            return ExitCode::from(1);
        }
        let core = match cli.core_space {
            Some(sp) => vyges_ifp::core_from_die_spacing(
                die,
                to_dbu(sp[0], dbu_f), to_dbu(sp[1], dbu_f),
                to_dbu(sp[2], dbu_f), to_dbu(sp[3], dbu_f),
            ),
            None => rect(cli.core),
        };
        (die, core)
    } else {
    match cli.utilization {
        None => (rect(cli.die), rect(cli.core)),
        Some(util) => {
            let sp = cli.core_space.expect("checked when the arguments were parsed");
            let aspect = cli.aspect_ratio.unwrap_or(1.0);   // upstream's default is 1.0
            // Upstream's validators, in its own order: IFP-12 then IFP-36, then the spacings.
            if util < 0.0 {
                eprintln!("vyges-ifp: IFP-0012 utilization must be non-negative ({util})");
                return ExitCode::from(1);
            }
            if aspect <= 0.0 {
                eprintln!("vyges-ifp: IFP-0036 aspect_ratio must be positive ({aspect})");
                return ExitCode::from(1);
            }
            // IFP-32..35, one per side, checked in MICRONS as upstream does.
            for (v, code, what) in [
                (sp[0], 32, "core_space_bottom"), (sp[1], 33, "core_space_top"),
                (sp[2], 34, "core_space_left"), (sp[3], 35, "core_space_right"),
            ] {
                if v < 0.0 {
                    eprintln!("vyges-ifp: IFP-00{code} {what} (um) must be non-negative ({v})");
                    return ExitCode::from(1);
                }
            }
            let (b, t, l, r) = (to_dbu(sp[0], dbu_f), to_dbu(sp[1], dbu_f),
                                to_dbu(sp[2], dbu_f), to_dbu(sp[3], dbu_f));
            vyges_events::emit(
                &vyges_events::Event::new(
                    "vyges-ifp",
                    vyges_events::Severity::Info,
                    format!("IFP-0107 Defining die area using utilization: {util:.2}% and \
                             aspect ratio: {aspect}."),
                )
                .with_code("IFP-DIE-FROM-UTILIZATION"),
            );
            let area = vyges_ifp::design_area(&instances);
            let die = vyges_ifp::die_from_utilization(area, util, aspect, b, t, l, r);
            // `makeDie` snaps every corner before the core is taken back off it.
            let snapped = Rect::new(
                vyges_ifp::snap_to_mfg_grid(die.x_min, grid),
                vyges_ifp::snap_to_mfg_grid(die.y_min, grid),
                vyges_ifp::snap_to_mfg_grid(die.x_max, grid),
                vyges_ifp::snap_to_mfg_grid(die.y_max, grid),
            );
            (snapped, vyges_ifp::core_from_die_spacing(snapped, b, t, l, r))
        }
    }
    };

    let mut p = match plan(
        die_in,
        core_in,
        &sites,
        cli.parity,
        &cli.flipped,
        grid,
        &instances,
    ) {
        Ok(p) => p,
        Err(e) => return refuse(e, dbu_f),
    };

    // ⛔ **`updateVoltageDomain` runs AFTER the core area and the row counts are settled**, and
    // that ordering is load-bearing: upstream's `makeUniformRows` ends with
    // `setCoreArea(computeCoreArea())` and prints IFP-0001 from the rows it made, and only then
    // does `makeRows` split them around the domains. Splitting earlier would change both the
    // reported core area and the row counts.
    // ⛔ The split rows are kept SEPARATE from the plan's own, because the core area and the
    // per-site counts are derived from the rows BEFORE the split -- see `apply`.
    let mut split_rows: Option<Vec<vyges_ifp::Row>> = None;
    let domains = voltage_domains(&db);
    if !domains.is_empty() {
        let heights: std::collections::HashMap<String, i32> =
            sites.iter().map(|s| (s.name.clone(), s.height)).collect();
        let pads: std::collections::HashSet<String> = sites
            .iter()
            .filter(|s| {
                db.site_get_class(&s.name).unwrap_or_default() == "PAD"
            })
            .map(|s| s.name.clone())
            .collect();
        split_rows = Some(vyges_ifp::split_rows_for_domains(
            p.rows.clone(),
            &domains,
            p.core_snapped,
            gap,
            &|site| heights.get(site).copied().unwrap_or(0),
            &|site| pads.contains(site),
        ));
    }

    let mut written: Option<String> = None;
    if !cli.dry_run {
        if let Err(e) = apply(&mut db, &p, split_rows.as_deref(), !cli.make_rows) {
            eprintln!("vyges-ifp: {e}");
            return ExitCode::from(2);
        }
        let out = cli.out_odb.clone().unwrap_or_else(|| cli.odb.clone());
        if let Err(e) = db.write(&out) {
            eprintln!("vyges-ifp: cannot write {out}: {e}");
            return ExitCode::from(2);
        }
        written = Some(out);
    }

    emit_events(&p, dbu_f, written.is_some(), db.num_insts(), &instances);

    let status = if cli.dry_run { "planned" } else { "applied" };
    let json = report_json(&p, dbu_f, status, written.as_deref());
    match cli.report.as_deref() {
        Some(f) => {
            if let Err(e) = std::fs::write(f, format!("{json}\n")) {
                eprintln!("vyges-ifp: cannot write {f}: {e}");
                return ExitCode::from(2);
            }
        }
        None => println!("{json}"),
    }
    ExitCode::SUCCESS
}

/// `make-tracks`: upstream `make_tracks` (`ifp/src/InitFloorplan.tcl:158`).
///
/// Two forms, both upstream's:
///   * no `--track` given  -> `ifp::make_layer_tracks`, i.e. every ROUTING layer with a non-zero
///     routing level, taking each layer's own LEF pitch and offset;
///   * `--track LAYER:xoff,xpitch,yoff,ypitch` (MICRONS) -> the one-layer form, which is what a
///     technology's `.tracks` file calls once per layer.
///
/// ⚠️ **A layer whose pitch is zero is SKIPPED with a warning** (IFP-56 upstream), never given an
/// empty grid — an empty grid compares equal to every other empty grid, so it would read as
/// agreement.
///
/// ⛔ **X pattern first, then Y, on the same grid** — `makeTracks` adds both to one `dbTrackGrid`
/// and creates it only if absent, so calling this twice for a layer ADDS patterns rather than
/// replacing them, exactly as upstream does.
fn make_tracks(args: &[String]) -> ExitCode {
    let mut path: Option<&str> = None;
    let mut out: Option<&str> = None;
    // LAYER -> (x_offset, x_pitch, y_offset, y_pitch) in microns
    let mut explicit: Vec<(String, [f64; 4])> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out-odb" => {
                i += 1;
                match args.get(i) {
                    Some(v) => out = Some(v),
                    None => {
                        eprintln!("vyges-ifp make-tracks: --out-odb needs a FILE");
                        return ExitCode::from(2);
                    }
                }
            }
            "--track" => {
                i += 1;
                let Some(spec) = args.get(i) else {
                    eprintln!("vyges-ifp make-tracks: --track needs LAYER:xoff,xpitch,yoff,ypitch");
                    return ExitCode::from(2);
                };
                let Some((layer, rest)) = spec.split_once(':') else {
                    eprintln!("vyges-ifp make-tracks: --track wants LAYER:xoff,xpitch,yoff,ypitch");
                    return ExitCode::from(2);
                };
                let f: Vec<&str> = rest.split(',').collect();
                if f.len() != 4 {
                    eprintln!("vyges-ifp make-tracks: --track wants four values, got {rest:?}");
                    return ExitCode::from(2);
                }
                let mut v = [0.0f64; 4];
                for (k, t) in f.iter().enumerate() {
                    match t.trim().parse::<f64>() {
                        Ok(x) => v[k] = x,
                        Err(_) => {
                            eprintln!("vyges-ifp make-tracks: not a number: {t:?}");
                            return ExitCode::from(2);
                        }
                    }
                }
                explicit.push((layer.to_string(), v));
            }
            a if a.starts_with("--") => {
                eprintln!("vyges-ifp make-tracks: unknown option {a}");
                return ExitCode::from(2);
            }
            a => path = Some(a),
        }
        i += 1;
    }

    let Some(path) = path else {
        eprintln!("vyges-ifp make-tracks: needs <design.odb>");
        return ExitCode::from(2);
    };

    let mut db = match vyges_opendb::Db::open(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vyges-ifp make-tracks: cannot read {path}: {e}");
            return ExitCode::from(2);
        }
    };

    let dbu = db.dbu_per_micron();
    // ⚠️ `manufacturing_grid` is Result<Option<_>>: absent means the tech declares none, which is
    // upstream's `hasManufacturingGrid() == false` branch — plain micron-to-DBU, no snapping.
    let mfg = db.manufacturing_grid().ok().flatten().unwrap_or(0);
    let die = (
        db.block_get_die_area_x_min(),
        db.block_get_die_area_y_min(),
        db.block_get_die_area_x_max(),
        db.block_get_die_area_y_max(),
    );
    let (dx, dy) = (die.2 - die.0, die.3 - die.1);

    // Build the work list: either the layers named on the command line, or every routing layer.
    let mut work: Vec<(String, i32, i32, i32, i32)> = Vec::new(); // layer, xoff, xpitch, yoff, ypitch
    let mut skipped: Vec<serde_json::Value> = Vec::new();

    if !explicit.is_empty() {
        for (layer, v) in &explicit {
            let g = |m: f64| vyges_ifp::microns_to_mfg_grid(m, dbu, mfg);
            work.push((layer.clone(), g(v[0]), g(v[1]), g(v[2]), g(v[3])));
        }
    } else {
        // ⛔ **The first CORE row's site height, in block row order** — upstream's
        // `makeTracksNonUniform` takes the FIRST match and breaks, so a design whose rows mix site
        // classes depends on which comes first. Computed once: it is a property of the block, and
        // every non-uniform layer reads the same value.
        let core_row_height = db.block_get_rows().into_iter().find_map(|row| {
            let site = db.row_get_site(&row);
            match db.site_get_class(&site).unwrap_or_default().as_str() {
                "CORE" => Some(db.site_get_height(&site)),
                _ => None,
            }
        });

        for (name, dir) in db.layers_with_direction().unwrap_or_default() {
            // Upstream's filter, both halves: ROUTING type AND a non-zero routing level.
            if db.layer_get_type(&name).unwrap_or_default() != "ROUTING"
                || db.layer_get_routing_level(&name) == 0
            {
                continue;
            }
            let (xp, yp) = (db.layer_get_pitch_x(&name), db.layer_get_pitch_y(&name));

            // ⛔ **LEF58_PITCH is tested BEFORE the zero-pitch warning**, and it wins: a layer
            // carrying FIRSTLASTPITCH never reaches the ordinary path. Upstream's order, and it
            // matters -- the non-uniform routine has no pitch guard of its own.
            let first_last_pitch = db.layer_get_first_last_pitch(&name);
            if first_last_pitch > 0 {
                if dir != "HORIZONTAL" {
                    eprintln!("vyges-ifp make-tracks: IFP-0044 Non horizontal layer {name} uses \
                               property LEF58_PITCH.");
                    return ExitCode::from(1);
                }
                let Some(row_h) = core_row_height else {
                    eprintln!("vyges-ifp make-tracks: IFP-0045 No routing Row found in layer \
                               {name}");
                    return ExitCode::from(1);
                };
                // ⚠️ Ours, not upstream's: its `(row_h - 2*flp) / y_pitch` would divide by zero
                // here. A tool must not fault on a technology file, so this is refused with a
                // reason -- recorded in the divergence register rather than left to trap.
                if yp == 0 {
                    eprintln!("vyges-ifp make-tracks: layer {name} carries LEF58_PITCH but no \
                               y pitch to space its tracks by.");
                    return ExitCode::from(1);
                }
                // Each origin is one ordinary `makeTracks` call, and every one of them passes the
                // ROW HEIGHT as the y pitch -- so the x pattern is added identically once per
                // origin, which is upstream's own output and not a duplicate to collapse.
                for origin_y in
                    vyges_ifp::non_uniform_track_origins(die.1, row_h, yp, first_last_pitch)
                {
                    work.push((name.clone(), db.layer_get_offset_x(&name), xp, origin_y, row_h));
                }
                continue;
            }

            if xp == 0 || yp == 0 {
                // Upstream IFP-56: warn, and generate NO tracks for this layer.
                vyges_events::emit(
                    &vyges_events::Event::new(
                        "vyges-ifp",
                        vyges_events::Severity::Warn,
                        format!("IFP-0056 No pitch found layer {name} so no tracks will be \
                                 generated."),
                    )
                    .with_code("IFP-TRACK-NO-PITCH")
                    .with_objects(vec![format!("layer:{name}")]),
                );
                skipped.push(serde_json::json!({ "layer": name, "why": "no pitch (IFP-56)" }));
                continue;
            }
            work.push((
                name.clone(),
                db.layer_get_offset_x(&name),
                xp,
                db.layer_get_offset_y(&name),
                yp,
            ));
        }
    }

    if work.is_empty() {
        eprintln!("vyges-ifp make-tracks: no routing layer produced a track pattern.");
        return ExitCode::from(3);
    }

    let mut made: Vec<serde_json::Value> = Vec::new();
    for (layer, xoff, xpitch, yoff, ypitch) in &work {
        // ⛔ **`layer_get_min_width` returns u32; every comparison it feeds is i32.** Widening
        // the arithmetic instead would change the guards on any layer near the die edge, and no
        // gate would see it — transcribe the reference's types, not just its logic.
        let min_width = db.layer_get_min_width(layer) as i32;
        // ⛔ **One call for the whole layer, because upstream's skip is a `return` from
        // `makeTracks` — not a per-axis decision.** The `return` sits above `findTrackGrid`, so an
        // offset past the die on EITHER axis leaves the layer with no grid at all. Planning the
        // two axes independently and skipping only the failing one emits exactly the right
        // warning and still writes a pattern upstream never creates, which is why no comparison
        // of log lines can see it. See `vyges_ifp::track_patterns`.
        let (px, py) = match vyges_ifp::track_patterns(
            die.0, dx, die.1, dy, *xoff, *xpitch, *yoff, *ypitch, min_width,
        ) {
            Ok(pair) => pair,
            Err(axis) => {
                // ⚠️ Upstream's own wording, verbatim. The conformance harness diffs emitted lines
                // against the `.ok` golden, so the golden's phrasing IS the contract.
                let (msg, code, why) = match axis {
                    vyges_ifp::TrackSkip::X => (
                        format!("IFP-0021 Track pattern for {layer} will be skipped due to \
                                 x_offset > die width."),
                        "IFP-TRACK-SKIP-X",
                        "x_offset > die width (IFP-21)",
                    ),
                    vyges_ifp::TrackSkip::Y => (
                        format!("IFP-0022 Track pattern for {layer} will be skipped due to \
                                 y_offset > die height."),
                        "IFP-TRACK-SKIP-Y",
                        "y_offset > die height (IFP-22)",
                    ),
                };
                vyges_events::emit(
                    &vyges_events::Event::new("vyges-ifp", vyges_events::Severity::Warn, msg)
                        .with_code(code)
                        .with_objects(vec![format!("layer:{layer}")]),
                );
                skipped.push(serde_json::json!({ "layer": layer, "why": why }));
                continue;
            }
        };
        // ⛔ X then Y, on one grid -- `makeTracks`'s order.
        if let Err(e) = db.add_track_pattern_x(layer, px.origin, px.count, px.step) {
            eprintln!("vyges-ifp make-tracks: {layer}: {e}");
            return ExitCode::from(1);
        }
        if let Err(e) = db.add_track_pattern_y(layer, py.origin, py.count, py.step) {
            eprintln!("vyges-ifp make-tracks: {layer}: {e}");
            return ExitCode::from(1);
        }
        // A layer is now all-or-nothing, so neither axis can be null here — a skipped layer
        // appears in `skipped` alone, exactly as upstream leaves it with no grid.
        made.push(serde_json::json!({
            "layer": layer,
            "x": {"origin": px.origin, "count": px.count, "step": px.step},
            "y": {"origin": py.origin, "count": py.count, "step": py.step},
        }));
    }

    let dest = out.unwrap_or(path);
    if let Err(e) = db.write(dest) {
        eprintln!("vyges-ifp make-tracks: cannot write {dest}: {e}");
        return ExitCode::from(2);
    }
    // ⚠️ PRETTY, like `run`'s hand-built report. `json!`'s Display is compact, and a chain that
    // prints one command's report as a block and the next one's as a single long line reads as
    // two different tools.
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "tool": "vyges-ifp",
        "command": "make-tracks",
        "status": "applied",
        "layers": made.len(),
        "tracks": made,
        "skipped": skipped,
        "odb_written": dest,
    })).expect("the make-tracks report is valid JSON"));
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 🔑 **The commit, not just the version.** Two binaries can share a version and differ by a
    // fix, so a bug report needs the build. build.rs prefers GITHUB_SHA on CI, which is what stops
    // a release being stamped -dirty by the untracked files a release run leaves behind.
    //
    // ⚠️ Answered before --describe, --help and any argument parsing: asking a binary what it is
    // must not depend on the rest of the command line being valid.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("vyges-ifp {} ({})", vyges_ifp::VERSION, env!("VYGES_GIT_SHA"));
        println!("{}", vyges_ifp::COPYRIGHT);
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--describe") {
        print!("{}", describe());
        return ExitCode::SUCCESS;
    }
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return if args.is_empty() {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
    }
    if args[0] == "make-rows" {
        return make_rows_cmd(&args[1..]);
    }
    if args[0] == "make-tracks" {
        return make_tracks(&args[1..]);
    }
    if args[0] != "run" {
        eprintln!("vyges-ifp: unknown command `{}`\n\n{USAGE}", args[0]);
        return ExitCode::from(2);
    }
    run(&args[1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rect_parses_from_either_separator_and_rejects_the_wrong_arity() {
        assert_eq!(
            parse_rect("0 0 1000 1000"),
            Some([0.0, 0.0, 1000.0, 1000.0])
        );
        assert_eq!(parse_rect("0,0,10.5,20.25"), Some([0.0, 0.0, 10.5, 20.25]));
        assert_eq!(
            parse_rect("0 0 1000"),
            None,
            "three numbers is not a rectangle"
        );
        assert_eq!(parse_rect("0 0 1000 1000 1000"), None, "nor is five");
        assert_eq!(parse_rect("0 0 x 1000"), None);
    }

    #[test]
    fn microns_convert_by_rounding_not_truncation() {
        // 0.0155 um at 1000 DBU/um is 15.5 DBU; truncating would lose half a DBU per edge and
        // the die would come out a DBU short.
        assert_eq!(to_dbu(0.0155, 1000.0), 16);
        assert_eq!(to_dbu(1000.0, 2000.0), 2_000_000);
        assert_eq!(to_dbu(-0.5, 1000.0), -500);
    }

    #[test]
    fn the_required_arguments_are_each_named_when_missing() {
        let base = [
            "d.odb",
            "--die-area",
            "0 0 10 10",
            "--core-area",
            "1 1 9 9",
            "--site",
            "S",
        ]
        .map(String::from);
        assert!(parse_args(&base).is_ok());

        // ⚠️ `--site` is named by UPSTREAM'S message (IFP-0035), not by our option spelling: it is
        // `parse_row_params` that demands it, and both commands reach that first.
        for (drop_at, expect) in [(1, "--die-area"), (3, "--core-area"), (5, "IFP-0035")] {
            let mut a: Vec<String> = base.to_vec();
            a.drain(drop_at..drop_at + 2);
            let e = parse_args(&a).expect_err("must refuse");
            assert!(e.contains(expect), "dropping {expect} said: {e}");
        }
        let e = parse_args(&base[1..]).expect_err("must refuse");
        assert!(e.contains(".odb"), "{e}");
    }

    #[test]
    fn a_value_taking_option_at_the_end_is_an_error_not_a_panic() {
        let a = ["d.odb", "--site"].map(String::from);
        assert!(parse_args(&a).expect_err("refuses").contains("--site"));
    }

    #[test]
    fn lists_split_on_either_separator() {
        assert_eq!(parse_list("a,b"), vec!["a", "b"]);
        assert_eq!(parse_list("a b"), vec!["a", "b"]);
        assert_eq!(parse_list(""), Vec::<String>::new());
    }

    #[test]
    fn the_descriptor_is_valid_json_and_declares_what_it_writes() {
        let d: serde_json::Value =
            serde_json::from_str(DESCRIBE).expect("descriptor is valid JSON");
        assert_eq!(d["name"], "ifp");
        assert_eq!(d["assertion"]["field"], "status");
        // ifp is the first engine that MUTATES the database, so the descriptor has to say so or
        // an orchestrator will treat the .odb as read-only and lose the floorplan.
        assert_eq!(d["produces"][0], "odb");
        let limits = d["provenance_limitations"].as_array().expect("an array");
        assert!(
            limits
                .iter()
                .any(|l| l.as_str().unwrap_or("").contains("utilization")),
            "the unimplemented utilization form must be declared"
        );
        assert!(limits
            .iter()
            .any(|l| l.as_str().unwrap_or("").contains("IN PLACE")));
    }

    #[test]
    fn the_report_is_valid_json_and_carries_both_unit_systems() {
        let site = Site::plain("S", 380, 2800);
        let p = plan(
            Rect::new(0, 0, 2_000_000, 2_000_000),
            Rect::new(200_000, 200_000, 1_800_000, 1_800_000),
            &[site],
            RowParity::None,
            &[],
            None,
            &[],
        )
        .expect("plans");
        let v: serde_json::Value =
            serde_json::from_str(&report_json(&p, 2000.0, "applied", Some("out.odb")))
                .expect("report is valid JSON");
        assert_eq!(v["status"], "applied");
        assert_eq!(v["rows"], 570);
        assert_eq!(v["core_area"]["dbu"][0], 200_260);
        assert_eq!(v["core_area"]["um"][0], 100.13);
        assert_eq!(v["core_was_snapped"], true);
        assert_eq!(v["odb_written"], "out.odb");
    }
}

#[cfg(test)]
mod pin_tests {
    use super::{describe, PIN_TOKEN};

    #[test]
    fn the_descriptor_reports_the_pin_this_binary_was_built_against() {
        let d = describe();
        assert!(
            !d.contains(PIN_TOKEN),
            "the pin placeholder survived into the output -- the substitution did not run"
        );
        let v: serde_json::Value =
            serde_json::from_str(&d).expect("the descriptor is still valid JSON once filled in");
        assert_eq!(
            v["openroad_pin"], super::CRATE_PIN,
            "the descriptor must report the pin this binary was actually built against"
        );
        assert_eq!(super::CRATE_PIN.len(), 40, "a full commit SHA, not an abbreviation");
    }

    /// ⛔ The whole point of inheriting the pin is that no engine carries one of its own.
    #[test]
    fn no_sha_is_hardcoded_anywhere_in_the_descriptor() {
        let raw = super::DESCRIBE;
        for tok in raw.split(|c: char| !c.is_ascii_hexdigit()) {
            assert!(
                tok.len() < 40,
                "{tok} looks like a hardcoded commit -- use the {PIN_TOKEN} placeholder"
            );
        }
    }
}
