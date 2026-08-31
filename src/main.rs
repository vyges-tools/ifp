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
  vyges physical ifp make-tracks <design.odb> [--track LAYER:xoff,xpitch,yoff,ypitch]... [--out-odb FILE]
  vyges physical ifp --describe
  vyges physical ifp --help

MAKE-TRACKS:
  Routing tracks over the die, from the technology's own pitches. With no --track, every ROUTING
  layer with a non-zero routing level is taken from the LEF; --track gives one layer explicitly,
  in MICRONS, which is the form a technology's .tracks file uses. Repeatable.

OPTIONS:
  --die-area 'x1 y1 x2 y2'   die rectangle, in MICRONS
  --core-area 'x1 y1 x2 y2'  core rectangle, in MICRONS
  --site NAME                the base site whose height sets the row pitch
  --additional-sites A,B     also tile rows for these sites (hybrid rows)
  --row-parity NONE|ODD|EVEN trim the row count to a parity (default NONE)
  --flip-sites A,B           shift the row-orientation phase for these sites
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
      "Implements the explicit-rectangle form of initialize_floorplan (die area plus core area). The utilization/aspect-ratio form, which derives the die from the placed cell area, is NOT implemented; ask for it with explicit rectangles.",
      "Areas are given in MICRONS, matching the upstream Tcl argument, and converted with the database's dbu_per_micron. A database with no DBU scale is an error rather than an assumed scale.",
      "The core's lower left is snapped UP to the site grid while the upper right is left where it was; the core area finally stored is what the rows COVER, not what was asked for. Both are upstream behaviors and both are load-bearing -- a caller that reads back the core area will not always get its own argument.",
      "Rows are named globally across sites (ROW_0, ROW_1, ...) rather than restarting per site, so adding a site renumbers the rows that follow it.",
      "Existing rows are cleared before the new ones are built. Anything already placed on the old row grid is not re-legalized by this engine.",
      "Written against the upstream ifp regression goldens at pin @OPENROAD_PIN@. The algorithm is reimplemented from the published behavior, not transliterated; where the two disagree the goldens are the arbiter.",
      "MEASURED against that suite at the same pinned commit: 23 cases reproduce every compared IFP-* line exactly, 2 fail, and 15 are not comparable (6 utilization form, 6 polygon floorplans, 3 that never call initialize_floorplan).",
      "HYBRID SITES are supported: a site with a row pattern tiles the core from that pattern (IFP-0049) and every hybrid site additionally gets rows spanning a whole pattern each (IFP-0050), offset to where its pattern occurs in the base pattern -- matching as written (R0) or reversed with orientations mirrored (MX). Row parity is REFUSED on a hybrid floorplan (IFP-0051), because parity would have to trim whole patterns rather than rows.",
      "Sites are visited in NAME order and deduplicated by name, not in the order given on the command line -- row numbering and log order both follow from this. The site set also includes sites used by placed instances that were never named as arguments (upstream addUsedSites), excluding blocks.",
      "Known gap (1 remaining) -- UPF POWER DOMAINS. Upstream's floorplan inserts power-domain instances and its instance census rises accordingly (16 to 40 on upf_test); this engine inserts none. All floorplan GEOMETRY matches exactly on those cases; what differs is the instance census that follows from the count -- IFP-0103, IFP-0104 and IFP-0105 together.",
      "A macro larger than the core area is refused with IFP-0002 before anything is snapped or written, matching upstream's ordering: the die checks come first, so a design with both an empty die and an oversized macro reports the die. Pads and covers are exempt, and a master with R90 symmetry is measured against the core's larger dimension because it is free to rotate.",
      "The instance census (IFP-0103 total instance area, IFP-0104 effective utilization) counts EVERY instance's master area, including the pads and covers the fit check skips -- it is a census of the design, not a question about the core. Utilization is omitted rather than printed as infinity when the core area is zero.",
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
}

/// `x1 y1 x2 y2`, whitespace- or comma-separated.
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

fn parse_args(args: &[String]) -> Result<Cli, String> {
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
                die = Some(
                    parse_rect(&v)
                        .ok_or_else(|| format!("--die-area wants 'x1 y1 x2 y2', got `{v}`"))?,
                );
            }
            "--core-area" => {
                let v = value()?;
                core = Some(
                    parse_rect(&v)
                        .ok_or_else(|| format!("--core-area wants 'x1 y1 x2 y2', got `{v}`"))?,
                );
            }
            "--site" => site = Some(value()?),
            "--additional-sites" => cli.additional = parse_list(&value()?),
            "--flip-sites" => cli.flipped = parse_list(&value()?),
            "--row-parity" => {
                let v = value()?;
                cli.parity = RowParity::parse(&v)
                    .ok_or_else(|| format!("--row-parity wants NONE, ODD or EVEN, got `{v}`"))?;
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

    cli.odb = odb.ok_or("`run` needs a path to a .odb")?;
    cli.die = die.ok_or("`run` needs --die-area")?;
    cli.core = core.ok_or("`run` needs --core-area")?;
    cli.site = site.ok_or("`run` needs --site")?;
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
/// Order matters — the core area is *derived* from the rows (R9), so it cannot be set first.
fn apply(db: &mut Db, p: &Plan) -> Result<(), String> {
    db.set_die_area(p.die.x_min, p.die.y_min, p.die.x_max, p.die.y_max)
        .map_err(|e| format!("cannot set the die area: {e}"))?;
    db.clear_rows()
        .map_err(|e| format!("cannot clear the existing rows: {e}"))?;
    for r in &p.rows {
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
    db.set_core_area_from_rows()
        .map_err(|e| format!("cannot set the core area: {e}"))
}

/// A refusal is a verdict about the design, not a crash: exit 1, and nothing was written.
fn refuse(e: PlanError, dbu: f64) -> ExitCode {
    use vyges_events::{Event, Severity};
    let code = match e {
        PlanError::NoRows => "IFP-NO-ROWS",
        PlanError::CoreNotInDie => "IFP-CORE-OUTSIDE-DIE",
        PlanError::EmptyDieArea => "IFP-EMPTY-DIE",
        PlanError::InstanceDoesNotFit { .. } => "IFP-INST-TOO-BIG",
        PlanError::ParityWithHybridRows => "IFP-PARITY-HYBRID",
        PlanError::IncompatibleSite { .. } => "IFP-SITE-INCOMPATIBLE",
        _ => "IFP-BAD-SITE",
    };
    let um = |v: i32| (v as f64) / dbu;
    let text = match e {
        // The ones the goldens name, in their words.
        PlanError::NoRows => "IFP-0065 No rows created in the core area.".to_string(),
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

fn run(args: &[String]) -> ExitCode {
    let cli = match parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vyges-ifp: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

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

    let p = match plan(
        rect(cli.die),
        rect(cli.core),
        &sites,
        cli.parity,
        &cli.flipped,
        grid,
        &instances,
    ) {
        Ok(p) => p,
        Err(e) => return refuse(e, dbu_f),
    };

    let mut written: Option<String> = None;
    if !cli.dry_run {
        if let Err(e) = apply(&mut db, &p) {
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
        for (name, _dir) in db.layers_with_direction().unwrap_or_default() {
            // Upstream's filter, both halves: ROUTING type AND a non-zero routing level.
            if db.layer_get_type(&name).unwrap_or_default() != "ROUTING"
                || db.layer_get_routing_level(&name) == 0
            {
                continue;
            }
            let (xp, yp) = (db.layer_get_pitch_x(&name), db.layer_get_pitch_y(&name));
            if xp == 0 || yp == 0 {
                // Upstream IFP-56: warn, and generate NO tracks for this layer.
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
        let px = vyges_ifp::track_pattern(die.0, dx, *xoff, *xpitch, min_width);
        let py = vyges_ifp::track_pattern(die.1, dy, *yoff, *ypitch, min_width);
        // ⛔ X then Y, on one grid -- `makeTracks`'s order.
        if let Some(p) = px {
            if let Err(e) = db.add_track_pattern_x(layer, p.origin, p.count, p.step) {
                eprintln!("vyges-ifp make-tracks: {layer}: {e}");
                return ExitCode::from(1);
            }
        } else {
            skipped.push(serde_json::json!({ "layer": layer, "why": "x_offset > die width (IFP-21)" }));
        }
        if let Some(p) = py {
            if let Err(e) = db.add_track_pattern_y(layer, p.origin, p.count, p.step) {
                eprintln!("vyges-ifp make-tracks: {layer}: {e}");
                return ExitCode::from(1);
            }
        } else {
            skipped.push(serde_json::json!({ "layer": layer, "why": "y_offset > die height (IFP-22)" }));
        }
        if px.is_some() || py.is_some() {
            made.push(serde_json::json!({
                "layer": layer,
                "x": px.map(|p| serde_json::json!({"origin": p.origin, "count": p.count, "step": p.step})),
                "y": py.map(|p| serde_json::json!({"origin": p.origin, "count": p.count, "step": p.step})),
            }));
        }
    }

    let dest = out.unwrap_or(path);
    if let Err(e) = db.write(dest) {
        eprintln!("vyges-ifp make-tracks: cannot write {dest}: {e}");
        return ExitCode::from(2);
    }
    println!("{}", serde_json::json!({
        "tool": "vyges-ifp",
        "command": "make-tracks",
        "status": "applied",
        "layers": made.len(),
        "tracks": made,
        "skipped": skipped,
        "odb_written": dest,
    }));
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

        for (drop_at, expect) in [(1, "--die-area"), (3, "--core-area"), (5, "--site")] {
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
