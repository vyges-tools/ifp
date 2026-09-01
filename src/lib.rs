// SPDX-License-Identifier: Apache-2.0
//! Floorplan initialization — die area, site-grid snapping, and rows.
//!
//! This crate is deliberately split in two:
//!
//! - **the planner** (this module) is pure arithmetic over integers in DBU. It takes the die and
//!   core rectangles, the site dimensions and the manufacturing grid, and returns a [`Plan`]:
//!   where every row goes and what the core area becomes. No database, no I/O, no globals.
//! - **the applier** ([`apply`]) walks that plan into the database through `vyges-opendb`.
//!
//! The split is what makes the rules testable against the reference values *without* an `.odb`
//! — and the rules are where the behaviour lives. Getting the arithmetic right off-grid, at a
//! parity boundary, or on a core that snaps is the whole job; writing rows is bookkeeping.
//!
//! # Provenance
//!
//! The rules implemented here are numbered **R1**
//! …**R11** are cited on each function). That spec was extracted by reading OpenROAD's
//! `InitFloorplan.cc`, which is the **reference implementation** for this behaviour; nothing is
//! copied from it. Where a rule looks arbitrary — the lower-left snapping up while the upper
//! right does not move, or the core area being replaced by what the rows cover — it is a
//! deliberate match to observable behaviour asserted by upstream's own regression goldens.

/// An axis-aligned rectangle in database units.
/// This crate's version, as Cargo knows it — the single number the whole suite is released on.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The copyright line `--version` prints.
pub const COPYRIGHT: &str = "© 2026 Vyges. All Rights Reserved.  https://vyges.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x_min: i32,
    pub y_min: i32,
    pub x_max: i32,
    pub y_max: i32,
}

impl Rect {
    pub fn new(x_min: i32, y_min: i32, x_max: i32, y_max: i32) -> Rect {
        Rect {
            x_min,
            y_min,
            x_max,
            y_max,
        }
    }
    pub fn dx(&self) -> i32 {
        self.x_max - self.x_min
    }
    pub fn dy(&self) -> i32 {
        self.y_max - self.y_min
    }
    pub fn is_empty(&self) -> bool {
        self.dx() <= 0 || self.dy() <= 0
    }
    /// Does this rectangle fully contain `other`?
    pub fn contains(&self, other: &Rect) -> bool {
        self.x_min <= other.x_min
            && self.y_min <= other.y_min
            && self.x_max >= other.x_max
            && self.y_max >= other.y_max
    }
}

/// One entry of a hybrid site's row pattern: which site, in which orientation, and how big it
/// is. Dimensions are carried here so the planner never has to look a site up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrientedSite {
    pub site: String,
    pub orient: String,
    pub width: i32,
    pub height: i32,
}

/// A site's footprint, in DBU.
///
/// A **hybrid** site carries a `row_pattern`: a repeating sequence of other sites that tiles the
/// core in its place. Its own `height` is the height of the whole pattern, which is what makes
/// the core snap work unchanged for both kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub name: String,
    pub width: i32,
    pub height: i32,
    /// Empty for an ordinary single-height site.
    pub row_pattern: Vec<OrientedSite>,
}

impl Site {
    /// An ordinary single-height site.
    pub fn plain(name: impl Into<String>, width: i32, height: i32) -> Site {
        Site {
            name: name.into(),
            width,
            height,
            row_pattern: Vec::new(),
        }
    }
    pub fn is_hybrid(&self) -> bool {
        !self.row_pattern.is_empty()
    }
}

/// How many rows to keep (**R5**).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowParity {
    #[default]
    None,
    Even,
    Odd,
}

impl RowParity {
    pub fn parse(s: &str) -> Option<RowParity> {
        match s.to_ascii_uppercase().as_str() {
            "NONE" => Some(RowParity::None),
            "EVEN" => Some(RowParity::Even),
            "ODD" => Some(RowParity::Odd),
            _ => None,
        }
    }
}

/// One row, as it will be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub site: String,
    pub x: i32,
    pub y: i32,
    /// `R0`/`MX` from the alternation of **R6**, or whatever the row pattern names (**R14**).
    pub orient: String,
    pub num_sites: i32,
    pub spacing: i32,
}

/// What the planner decided, before anything touches the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub die: Rect,
    /// The core as asked for.
    pub core_requested: Rect,
    /// The core after **R3** — lower left snapped up to the site grid.
    pub core_snapped: Rect,
    pub rows: Vec<Row>,
    /// What the rows actually cover (**R9**) — this is what the core area becomes.
    pub core_final: Rect,
    /// Rows created per site, in the order the sites were given: `(site name, count)`.
    pub rows_per_site: Vec<(String, i32)>,
    /// Set only for a hybrid floorplan: `(base site name, rows built from its pattern)`. Its
    /// presence is what distinguishes the two constructions, and the two report differently.
    pub pattern_rows: Option<(String, i32)>,
}

impl Plan {
    /// Did **R3** move the core's lower left? Determines whether IFP-28 is reported.
    pub fn core_was_snapped(&self) -> bool {
        self.core_snapped.x_min != self.core_requested.x_min
            || self.core_snapped.y_min != self.core_requested.y_min
    }
}

/// One placed instance, reduced to what the floorplan cares about: how big its master is, and
/// the two master flags that change whether "big" is a problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub name: String,
    /// Master width and height, in DBU.
    pub width: i32,
    pub height: i32,
    /// Pads and covers sit outside the core by definition and are exempt from the fit check.
    pub is_pad: bool,
    pub is_cover: bool,
    /// A master free to rotate 90° only has to fit the core's *larger* dimension.
    pub symmetry_r90: bool,
}

/// **R12** — the total area of every instance's master, in DBU², for the census (upstream
/// IFP-0103) and the utilization it feeds (IFP-0104).
///
/// Every instance counts, including the pads and covers the fit check skips: this is a census of
/// the design, not a question about the core. Accumulated in `f64` as upstream does, so the two
/// agree bit for bit on large designs rather than diverging by an integer rounding.
pub fn design_area(instances: &[Instance]) -> f64 {
    instances
        .iter()
        .map(|i| f64::from(i.height) * f64::from(i.width))
        .sum()
}

/// What a plan cannot be made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// A master is larger than the core area (upstream IFP-0002). Dimensions are DBU; the caller
    /// converts to microns, since only it knows the database's scale.
    InstanceDoesNotFit {
        name: String,
        width: i32,
        height: i32,
        core: Rect,
    },
    /// The die area has no extent (upstream IFP-63).
    EmptyDieArea,
    /// The die does not contain the core (upstream IFP-55).
    CoreNotInDie,
    /// A site has zero width or height — nothing can be tiled with it.
    DegenerateSite(String),
    /// An additional site's height is not a multiple of the base site's (upstream IFP-54).
    SiteHeightNotMultiple { site: String, base: String },
    /// No site produced a single row (upstream IFP-65).
    NoRows,
    /// Row parity was asked for on a hybrid floorplan (upstream IFP-51).
    ParityWithHybridRows,
    /// A site's row pattern does not occur in the base site's (upstream IFP-48).
    IncompatibleSite { site: String, base: String },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::EmptyDieArea => write!(f, "die area is empty; cannot build rows"),
            PlanError::CoreNotInDie => write!(f, "die area must contain the core area"),
            PlanError::DegenerateSite(s) => write!(f, "site {s} has zero width or height"),
            PlanError::SiteHeightNotMultiple { site, base } => {
                write!(
                    f,
                    "site {site} height is not a multiple of site {base} height"
                )
            }
            PlanError::NoRows => write!(f, "no rows created in the core area"),
            PlanError::ParityWithHybridRows => {
                write!(
                    f,
                    "constraining row parity is not supported for hybrid rows"
                )
            }
            PlanError::IncompatibleSite { site, base } => {
                write!(f, "site {site} is incompatible with site {base}")
            }
            PlanError::InstanceDoesNotFit {
                name,
                width,
                height,
                core,
            } => write!(
                f,
                "{name} ({width}, {height} DBU) does not fit in the core area: \
                 ({}, {}) - ({}, {})",
                core.x_min, core.y_min, core.x_max, core.y_max
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// **R13** — refuse a core too small to hold one of the design's masters (upstream IFP-0002).
///
/// Three details are upstream's and all three matter:
///
/// - **pads and covers are exempt** — they belong outside the core, so their size says nothing
///   about whether the core is big enough;
/// - a master with **R90 symmetry** is free to rotate, so it only has to fit the core's *larger*
///   dimension, not width-against-width;
/// - the core checked is the one **as asked for**, before any site-grid snapping, which is why
///   the reported rectangle is the caller's own numbers.
///
/// Returns the first offender, matching upstream's stop-on-first-error.
pub fn check_instance_dimensions(instances: &[Instance], core: Rect) -> Result<(), PlanError> {
    let max_d = core.dx().max(core.dy());
    for i in instances {
        if i.is_pad || i.is_cover {
            continue;
        }
        let fails = if i.symmetry_r90 {
            i.width.max(i.height) > max_d
        } else {
            i.width > core.dx() || i.height > core.dy()
        };
        if fails {
            return Err(PlanError::InstanceDoesNotFit {
                name: i.name.clone(),
                width: i.width,
                height: i.height,
                core,
            });
        }
    }
    Ok(())
}

/// **R1** — snap a coordinate to the manufacturing grid.
///
/// Nearest, **not** floor or ceil, and a no-op when the tech declares no grid. Note this is a
/// *different* rounding from the core snap in [`snap_core_lower_left`]; the two are easy to
/// conflate and upstream uses each in exactly one place.
pub fn snap_to_mfg_grid(coord: i32, grid: Option<i32>) -> i32 {
    match grid {
        Some(g) if g > 0 => {
            let q = (coord as f64) / (g as f64);
            (q.round() as i32) * g
        }
        _ => coord,
    }
}

/// `ceil(a / b)` for positive `b`, on integers.
fn div_ceil(a: i32, b: i32) -> i32 {
    debug_assert!(b > 0);
    if a >= 0 {
        (a + b - 1) / b
    } else {
        -((-a) / b)
    }
}

/// **R5** — trim a row count to the requested parity.
pub fn apply_row_parity(rows_y: i32, parity: RowParity) -> i32 {
    match parity {
        RowParity::None => rows_y,
        RowParity::Even => (rows_y / 2) * 2,
        RowParity::Odd if rows_y > 0 && rows_y % 2 == 0 => rows_y - 1,
        RowParity::Odd => rows_y,
    }
}

/// **R3** — snap the core's lower left **up** to the site grid; the upper right does not move.
///
/// The asymmetry is the point, and it is not an accident of implementation: the lower left is
/// rounded up so rows start on a legal site boundary, while the upper right is left alone
/// because the row count (**R4**) is what decides where the core really ends.
///
/// Applied only when both lower-left coordinates are non-negative, matching upstream — a core
/// placed at a negative origin is left as given.
pub fn snap_core_lower_left(core: Rect, site: &Site) -> Rect {
    if core.x_min < 0 || core.y_min < 0 {
        return core;
    }
    Rect {
        x_min: div_ceil(core.x_min, site.width) * site.width,
        y_min: div_ceil(core.y_min, site.height) * site.height,
        x_max: core.x_max,
        y_max: core.y_max,
    }
}

/// Build the floorplan plan: **R2** → **R3** → **R4** → **R6** → **R7** → **R8** → **R9**.
///
/// `sites` is the base site followed by any additional ones; rows are generated for each, in
/// order, and row names number **globally** (R7) rather than restarting per site.
///
/// `flipped` names sites whose row-orientation phase is shifted (R6).
///
/// `instances` are checked against the core (**R13**) at exactly the point upstream checks them:
/// after the die tests, before any snapping. The order is load-bearing — a design with both an
/// empty die and an oversized macro must report the die, which is the error a caller can act on.
/// Pass `&[]` to plan geometry alone.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    die: Rect,
    core: Rect,
    sites: &[Site],
    parity: RowParity,
    flipped: &[String],
    mfg_grid: Option<i32>,
    instances: &[Instance],
) -> Result<Plan, PlanError> {
    let die = Rect {
        x_min: snap_to_mfg_grid(die.x_min, mfg_grid),
        y_min: snap_to_mfg_grid(die.y_min, mfg_grid),
        x_max: snap_to_mfg_grid(die.x_max, mfg_grid),
        y_max: snap_to_mfg_grid(die.y_max, mfg_grid),
    };
    if die.is_empty() {
        return Err(PlanError::EmptyDieArea);
    }
    if !die.contains(&core) {
        return Err(PlanError::CoreNotInDie);
    }
    check_instance_dimensions(instances, core)?;
    let base = sites.first().ok_or(PlanError::NoRows)?;
    for s in sites {
        if s.width <= 0 || s.height <= 0 {
            return Err(PlanError::DegenerateSite(s.name.clone()));
        }
    }
    // Upstream keys the sites in a std::map, so they are visited in NAME order and deduplicated
    // by name -- not in the order they were given on the command line. Both the row NUMBERING
    // and the order of the log lines follow from this, and the goldens assert both.
    let mut ordered: Vec<&Site> = sites.iter().collect();
    ordered.sort_by(|a, b| a.name.cmp(&b.name));
    ordered.dedup_by(|a, b| a.name == b.name);

    // The core snap is the SAME for both paths — a hybrid site's height is already the whole
    // pattern's height, so `divCeil` against it lands on a pattern boundary without a special
    // case. (This is why the hybrid goldens snap y to a different multiple, not a different rule.)
    let core_snapped = snap_core_lower_left(core, base);

    if base.is_hybrid() {
        // R15: parity would have to trim whole patterns, not rows, so upstream refuses (IFP-51)
        // rather than silently trimming the wrong unit.
        if parity != RowParity::None {
            return Err(PlanError::ParityWithHybridRows);
        }
        return plan_hybrid(die, core, core_snapped, &ordered, base);
    }

    // R10 applies only to the uniform path: a hybrid site's compatibility is decided by pattern
    // matching (R14), not by dividing heights, and requiring both would reject legal hybrids.
    for s in &ordered {
        if s.height % base.height != 0 {
            return Err(PlanError::SiteHeightNotMultiple {
                site: s.name.clone(),
                base: base.name.clone(),
            });
        }
    }

    // R4: the horizontal count comes from the BASE site and is shared by every row.
    let rows_x = core_snapped.dx() / base.width;

    let mut rows = Vec::new();
    let mut rows_per_site = Vec::new();
    for site in &ordered {
        let rows_y = apply_row_parity(core_snapped.dy() / site.height, parity);
        let flip = i32::from(flipped.iter().any(|f| f == &site.name));
        for r in 0..rows_y {
            rows.push(Row {
                // R7: numbering is global, so it continues across sites.
                name: format!("ROW_{}", rows.len()),
                site: site.name.clone(),
                x: core_snapped.x_min,
                y: core_snapped.y_min + r * site.height,
                // R6: alternate, with `flip` shifting the phase.
                orient: if (r + flip) % 2 == 0 { "R0" } else { "MX" }.to_string(),
                num_sites: rows_x,
                spacing: base.width,
            });
        }
        rows_per_site.push((site.name.clone(), rows_y));
    }
    if rows.is_empty() {
        return Err(PlanError::NoRows);
    }

    // R9: the core area becomes what the rows cover, not what was asked for.
    let core_final = core_area_of(&rows, &rows_per_site, sites, core_snapped);

    Ok(Plan {
        die,
        core_requested: core,
        core_snapped,
        rows,
        core_final,
        rows_per_site,
        pattern_rows: None,
    })
}

/// Mirror an orientation across X — odb's `dbOrientType::flipX`.
///
/// Used to test whether a site's pattern matches the base's *upside down*, which is how a
/// library expresses the same row sequence built from the other end.
pub fn flip_x(orient: &str) -> &'static str {
    match orient {
        "R0" => "MX",
        "MX" => "R0",
        "R180" => "MY",
        "MY" => "R180",
        "R90" => "MYR90",
        "MYR90" => "R90",
        "R270" => "MXR90",
        "MXR90" => "R270",
        _ => "R0",
    }
}

/// Does `pattern` occur in `base`, read cyclically? If so, how far up does it start?
///
/// The offset is the summed height of the base entries *before* the match, which is where rows
/// of that site have to begin so they land on the same boundaries the base pattern laid down.
fn match_pattern(base: &[OrientedSite], pattern: &[OrientedSite]) -> Option<i32> {
    let first = pattern.first()?;
    let start = base
        .iter()
        .position(|e| e.site == first.site && e.orient == first.orient)?;
    for (k, want) in pattern.iter().enumerate() {
        let got = &base[(start + k) % base.len()];
        if got.site != want.site || got.orient != want.orient {
            return None;
        }
    }
    Some(base[..start].iter().map(|e| e.height).sum())
}

/// **R14** — where `site`'s rows start inside `base`'s pattern, and in which orientation.
///
/// A site matches either as written (`R0`) or reversed with every orientation mirrored (`MX`) —
/// the same sequence built from the other end. Neither matching is upstream's IFP-48.
pub fn pattern_offset(base: &Site, site: &Site) -> Option<(i32, &'static str)> {
    if let Some(off) = match_pattern(&base.row_pattern, &site.row_pattern) {
        return Some((off, "R0"));
    }
    let flipped: Vec<OrientedSite> = site
        .row_pattern
        .iter()
        .rev()
        .map(|e| OrientedSite {
            orient: flip_x(&e.orient).to_string(),
            ..e.clone()
        })
        .collect();
    match_pattern(&base.row_pattern, &flipped).map(|off| (off, "MX"))
}

/// Build a hybrid floorplan: the base pattern tiles the core, then every hybrid site gets its
/// own rows aligned to where it appears in that pattern.
///
/// Two sets of rows are produced, and both are real: the pattern's *member* sites (IFP-0049) and
/// the hybrid sites themselves (IFP-0050), the latter spanning a whole pattern each.
fn plan_hybrid(
    die: Rect,
    core_requested: Rect,
    core: Rect,
    sites: &[&Site],
    base: &Site,
) -> Result<Plan, PlanError> {
    let pattern = &base.row_pattern;
    // Width comes from the pattern's FIRST site, not from the hybrid site itself, and every row
    // shares it — the pattern varies in height, never in width.
    let site_width = pattern[0].width;
    if site_width <= 0 {
        return Err(PlanError::DegenerateSite(pattern[0].site.clone()));
    }
    let row_width = core.dx() / site_width;

    let mut rows: Vec<Row> = Vec::new();
    let mut y = core.y_min;
    let mut r = 0usize;
    loop {
        let e = &pattern[r % pattern.len()];
        if y + e.height > core.y_max {
            break;
        }
        rows.push(Row {
            name: format!("ROW_{r}"),
            site: e.site.clone(),
            x: core.x_min,
            y,
            orient: e.orient.clone(),
            num_sites: row_width,
            spacing: site_width,
        });
        y += e.height;
        r += 1;
    }
    let pattern_rows = r as i32;

    // `sites` arrives in name order (see plan()); only the hybrid ones get their own rows,
    // because a plain member of the pattern is already covered by the loop above.
    let hybrids = sites.iter().filter(|s| s.is_hybrid());

    let mut rows_per_site = Vec::new();
    for site in hybrids {
        let (offset, orient) =
            pattern_offset(base, site).ok_or_else(|| PlanError::IncompatibleSite {
                site: site.name.clone(),
                base: base.name.clone(),
            })?;
        let mut y = core.y_min + offset;
        let mut n = 0;
        while y + site.height <= core.y_max {
            rows.push(Row {
                name: format!("ROW_{}", rows.len()),
                site: site.name.clone(),
                x: core.x_min,
                y,
                orient: orient.to_string(),
                num_sites: row_width,
                spacing: site_width,
            });
            y += site.height;
            n += 1;
        }
        rows_per_site.push((site.name.clone(), n));
    }

    if rows.is_empty() {
        return Err(PlanError::NoRows);
    }
    let core_final = Rect {
        x_min: core.x_min,
        y_min: core.y_min,
        x_max: core.x_min + row_width * site_width,
        y_max: rows.iter().map(|r| r.y).max().unwrap_or(core.y_min)
            + rows
                .iter()
                .max_by_key(|r| r.y)
                .map(|r| height_of(r, sites, pattern))
                .unwrap_or(0),
    };

    Ok(Plan {
        die,
        core_requested,
        core_snapped: core,
        rows,
        core_final,
        rows_per_site,
        pattern_rows: Some((base.name.clone(), pattern_rows)),
    })
}

/// The height of whatever site a row was built from — it may be a hybrid site or one of the
/// pattern's members, and only the two together cover every row.
fn height_of(row: &Row, sites: &[&Site], pattern: &[OrientedSite]) -> i32 {
    sites
        .iter()
        .find(|s| s.name == row.site)
        .map(|s| s.height)
        .or_else(|| {
            pattern
                .iter()
                .find(|e| e.site == row.site)
                .map(|e| e.height)
        })
        .unwrap_or(0)
}

/// **R9** — the bounding box the rows cover.
fn core_area_of(rows: &[Row], per_site: &[(String, i32)], sites: &[Site], snapped: Rect) -> Rect {
    let base = &sites[0];
    let rows_x = rows.first().map(|r| r.num_sites).unwrap_or(0);
    // Tallest stack across the sites: each site's rows start at the same y_min.
    let top = per_site
        .iter()
        .filter_map(|(name, n)| {
            sites
                .iter()
                .find(|s| &s.name == name)
                .map(|s| snapped.y_min + n * s.height)
        })
        .max()
        .unwrap_or(snapped.y_min);
    Rect {
        x_min: snapped.x_min,
        y_min: snapped.y_min,
        x_max: snapped.x_min + rows_x * base.width,
        y_max: top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nangate45's `FreePDK45_38x28_10R_NP_162NW_34O`: 0.19 × 1.4 µm at 2000 DBU/µm.
    fn nangate_site() -> Site {
        Site::plain("FreePDK45_38x28_10R_NP_162NW_34O", 380, 2800)
    }

    /// 🔑 THE REFERENCE CASE. Upstream's `init_floorplan1`: die 0..1000 µm, core 100..900 µm.
    /// Every number below is asserted by `init_floorplan1.ok`, so this test is the spec's
    /// worked example expressed in code — if it fails, the arithmetic is wrong, not the golden.
    #[test]
    fn the_reference_case_reproduces_every_number_in_the_golden() {
        let site = nangate_site();
        let p = plan(
            Rect::new(0, 0, 2_000_000, 2_000_000),
            Rect::new(200_000, 200_000, 1_800_000, 1_800_000),
            std::slice::from_ref(&site),
            RowParity::None,
            &[],
            None,
            &[],
        )
        .expect("plans");

        // "Core area lower left (100.000, 100.000) snapped to (100.130, 100.800)."
        assert_eq!(
            (p.core_snapped.x_min, p.core_snapped.y_min),
            (200_260, 201_600)
        );
        assert!(p.core_was_snapped(), "IFP-28 applies here");

        // "Added 570 rows of 4209 site FreePDK45_38x28_10R_NP_162NW_34O."
        assert_eq!(p.rows.len(), 570);
        assert_eq!(p.rows[0].num_sites, 4209);
        assert_eq!(p.rows_per_site, vec![(site.name.clone(), 570)]);

        // "Core BBox: ( 100.130 100.800 ) ( 899.840 898.800 )" — what the ROWS cover (R9).
        assert_eq!(
            p.core_final,
            Rect::new(200_260, 201_600, 1_799_680, 1_797_600)
        );
    }

    #[test]
    fn the_upper_right_does_not_move_when_the_lower_left_snaps() {
        // The asymmetry of R3, isolated: a from-scratch implementation that "snapped the core to
        // the site grid" symmetrically would pass the row count and fail the core bbox.
        let site = nangate_site();
        let core = Rect::new(200_000, 200_000, 1_800_000, 1_800_000);
        let s = snap_core_lower_left(core, &site);
        assert!(
            s.x_min > core.x_min && s.y_min > core.y_min,
            "lower left rounds UP"
        );
        assert_eq!(
            (s.x_max, s.y_max),
            (core.x_max, core.y_max),
            "upper right is untouched"
        );
    }

    #[test]
    fn an_already_aligned_core_is_left_alone() {
        let site = nangate_site();
        let core = Rect::new(380 * 10, 2800 * 3, 380 * 100, 2800 * 30);
        assert_eq!(snap_core_lower_left(core, &site), core);
        let p = plan(
            Rect::new(0, 0, 10_000_000, 10_000_000),
            core,
            &[site],
            RowParity::None,
            &[],
            None,
            &[],
        )
        .unwrap();
        assert!(
            !p.core_was_snapped(),
            "nothing moved, so IFP-28 must not be reported"
        );
    }

    #[test]
    fn row_parity_trims_the_count_and_never_grows_it() {
        for n in 0..8 {
            assert_eq!(apply_row_parity(n, RowParity::None), n);
            let e = apply_row_parity(n, RowParity::Even);
            assert!(e % 2 == 0 && e <= n, "even: {n} -> {e}");
            let o = apply_row_parity(n, RowParity::Odd);
            assert!(o <= n, "odd never grows: {n} -> {o}");
            if n > 0 {
                assert_eq!(o % 2, 1, "odd: {n} -> {o}");
            }
        }
        assert_eq!(
            apply_row_parity(0, RowParity::Odd),
            0,
            "0 stays 0 — there is no odd below it"
        );
    }

    #[test]
    fn orientation_alternates_and_flipping_shifts_the_phase() {
        let site = nangate_site();
        let die = Rect::new(0, 0, 10_000_000, 10_000_000);
        let core = Rect::new(0, 0, 380 * 50, 2800 * 4);
        let plain = plan(
            die,
            core,
            std::slice::from_ref(&site),
            RowParity::None,
            &[],
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            plain
                .rows
                .iter()
                .map(|r| r.orient.as_str())
                .collect::<Vec<_>>(),
            vec!["R0", "MX", "R0", "MX"]
        );
        let flipped = plan(
            die,
            core,
            std::slice::from_ref(&site),
            RowParity::None,
            std::slice::from_ref(&site.name),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            flipped
                .rows
                .iter()
                .map(|r| r.orient.as_str())
                .collect::<Vec<_>>(),
            vec!["MX", "R0", "MX", "R0"],
            "flipping a site shifts the phase, it does not reverse the sequence"
        );
    }

    #[test]
    fn row_names_number_globally_across_sites() {
        // R7: two sites, one numbering. Restarting per site would collide on ROW_0.
        let a = Site::plain("A", 100, 200);
        let b = Site::plain("B", 100, 400);
        let die = Rect::new(0, 0, 1_000_000, 1_000_000);
        let core = Rect::new(0, 0, 1000, 1200);
        let p = plan(die, core, &[a, b], RowParity::None, &[], None, &[]).unwrap();
        let names: Vec<&str> = p.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ROW_0", "ROW_1", "ROW_2", "ROW_3", "ROW_4", "ROW_5", "ROW_6", "ROW_7", "ROW_8"]
        );
        assert_eq!(
            p.rows_per_site,
            vec![("A".to_string(), 6), ("B".to_string(), 3)]
        );
    }

    #[test]
    fn the_manufacturing_grid_rounds_to_nearest_not_up() {
        // R1 vs R3 — the two snaps round differently, which is exactly the pair to get wrong.
        assert_eq!(
            snap_to_mfg_grid(104, Some(10)),
            100,
            "nearest rounds down here"
        );
        assert_eq!(snap_to_mfg_grid(106, Some(10)), 110);
        assert_eq!(
            snap_to_mfg_grid(105, Some(10)),
            110,
            "halfway rounds away from zero"
        );
        assert_eq!(
            snap_to_mfg_grid(104, None),
            104,
            "no grid declared, no snap"
        );
        assert_eq!(
            snap_to_mfg_grid(104, Some(0)),
            104,
            "a zero grid is 'none', not a divide by zero"
        );
    }

    fn inst(name: &str, w: i32, h: i32) -> Instance {
        Instance {
            name: name.into(),
            width: w,
            height: h,
            is_pad: false,
            is_cover: false,
            symmetry_r90: false,
        }
    }

    /// A two-entry pattern: A upright then B mirrored, 100 + 300 DBU tall, so the pattern is 400.
    fn hybrid_pair() -> (Site, Site, Site) {
        let a = Site::plain("A", 50, 100);
        let b = Site::plain("B", 50, 300);
        let base = Site {
            name: "H".into(),
            width: 50,
            height: 400, // a hybrid site's height IS the pattern's height
            row_pattern: vec![
                OrientedSite {
                    site: "A".into(),
                    orient: "R0".into(),
                    width: 50,
                    height: 100,
                },
                OrientedSite {
                    site: "B".into(),
                    orient: "MX".into(),
                    width: 50,
                    height: 300,
                },
            ],
        };
        (base, a, b)
    }

    #[test]
    fn a_hybrid_floorplan_builds_pattern_rows_and_hybrid_rows_both() {
        // The two row sets are not alternatives: the pattern's member sites tile the core, AND
        // the hybrid site gets its own rows spanning a whole pattern each.
        let (base, ..) = hybrid_pair();
        let p = plan(
            Rect::new(0, 0, 10_000, 10_000),
            Rect::new(0, 0, 5_000, 4_000),
            std::slice::from_ref(&base),
            RowParity::None,
            &[],
            None,
            &[],
        )
        .expect("plans");

        // 4000 tall / 400 per pattern = 10 patterns = 20 member rows, and 10 hybrid rows.
        assert_eq!(p.pattern_rows, Some(("H".into(), 20)));
        assert_eq!(p.rows_per_site, vec![("H".to_string(), 10)]);
        assert_eq!(p.rows.len(), 30);

        // The member rows alternate A(100) then B(300), taking their orientation from the
        // pattern rather than from the R6 alternation.
        assert_eq!(
            (
                p.rows[0].site.as_str(),
                p.rows[0].y,
                p.rows[0].orient.as_str()
            ),
            ("A", 0, "R0")
        );
        assert_eq!(
            (
                p.rows[1].site.as_str(),
                p.rows[1].y,
                p.rows[1].orient.as_str()
            ),
            ("B", 100, "MX")
        );
        assert_eq!((p.rows[2].site.as_str(), p.rows[2].y), ("A", 400));

        // Row numbering continues across both sets — ROW_0..ROW_19 then ROW_20..
        assert_eq!(p.rows[19].name, "ROW_19");
        assert_eq!(
            (p.rows[20].name.as_str(), p.rows[20].site.as_str()),
            ("ROW_20", "H")
        );
        // Width comes from the PATTERN's first site, not the hybrid site.
        assert!(p.rows.iter().all(|r| r.num_sites == 100 && r.spacing == 50));
    }

    #[test]
    fn a_partial_pattern_at_the_top_is_not_built() {
        // The core is 500 tall: one full pattern (400) plus room for A (100) but not B (300).
        // Upstream stops mid-pattern rather than overflowing, so the last B is simply absent.
        let (base, ..) = hybrid_pair();
        let p = plan(
            Rect::new(0, 0, 10_000, 10_000),
            Rect::new(0, 0, 5_000, 500),
            std::slice::from_ref(&base),
            RowParity::None,
            &[],
            None,
            &[],
        )
        .expect("plans");
        assert_eq!(
            p.pattern_rows,
            Some(("H".into(), 3)),
            "A, B, then A again — B would overflow"
        );
        assert_eq!(
            p.rows_per_site,
            vec![("H".to_string(), 1)],
            "only one whole pattern fits"
        );
        assert_eq!(
            p.core_final.y_max, 500,
            "the last A ends exactly at the top"
        );
    }

    #[test]
    fn a_second_hybrid_site_is_offset_to_where_its_pattern_starts() {
        // H2's pattern is the SECOND half of H's, so its rows must begin 100 DBU up — at B,
        // not at the core floor. Starting both at y_min would put H2 on the wrong boundary.
        let (base, ..) = hybrid_pair();
        let h2 = Site {
            name: "H2".into(),
            width: 50,
            height: 400,
            row_pattern: vec![
                OrientedSite {
                    site: "B".into(),
                    orient: "MX".into(),
                    width: 50,
                    height: 300,
                },
                OrientedSite {
                    site: "A".into(),
                    orient: "R0".into(),
                    width: 50,
                    height: 100,
                },
            ],
        };
        assert_eq!(pattern_offset(&base, &h2), Some((100, "R0")));

        let p = plan(
            Rect::new(0, 0, 10_000, 10_000),
            Rect::new(0, 0, 5_000, 4_000),
            &[base, h2],
            RowParity::None,
            &[],
            None,
            &[],
        )
        .expect("plans");
        let h2_rows: Vec<&Row> = p.rows.iter().filter(|r| r.site == "H2").collect();
        assert_eq!(
            h2_rows[0].y, 100,
            "H2 starts where its pattern starts inside H's"
        );
        assert_eq!(
            h2_rows.len(),
            9,
            "starting 100 up, one fewer whole pattern fits below the top"
        );
    }

    #[test]
    fn a_pattern_that_matches_upside_down_is_accepted_as_mirrored() {
        // Reversed order with every orientation flipped is the same sequence read the other way,
        // so it matches — as MX. Rejecting it would refuse a legal library.
        let (base, ..) = hybrid_pair();
        let mirrored = Site {
            name: "M".into(),
            width: 50,
            height: 400,
            row_pattern: vec![
                OrientedSite {
                    site: "B".into(),
                    orient: "R0".into(),
                    width: 50,
                    height: 300,
                },
                OrientedSite {
                    site: "A".into(),
                    orient: "MX".into(),
                    width: 50,
                    height: 100,
                },
            ],
        };
        assert_eq!(pattern_offset(&base, &mirrored), Some((0, "MX")));

        // A pattern naming a site the base never mentions matches neither way.
        let alien = Site {
            name: "X".into(),
            width: 50,
            height: 100,
            row_pattern: vec![OrientedSite {
                site: "Z".into(),
                orient: "R0".into(),
                width: 50,
                height: 100,
            }],
        };
        assert_eq!(pattern_offset(&base, &alien), None);
    }

    #[test]
    fn flipping_an_orientation_is_its_own_inverse() {
        for o in ["R0", "MX", "R90", "MYR90", "R180", "MY", "R270", "MXR90"] {
            assert_eq!(flip_x(flip_x(o)), o, "flipping {o} twice must return it");
            assert_ne!(flip_x(o), o, "flipping {o} must change it");
        }
    }

    #[test]
    fn row_parity_is_refused_on_a_hybrid_floorplan() {
        // Parity would have to trim whole patterns, not rows. Upstream refuses (IFP-51) rather
        // than trimming the wrong unit, and silently ignoring the flag would be worse.
        let (base, ..) = hybrid_pair();
        for parity in [RowParity::Even, RowParity::Odd] {
            assert_eq!(
                plan(
                    Rect::new(0, 0, 10_000, 10_000),
                    Rect::new(0, 0, 5_000, 4_000),
                    std::slice::from_ref(&base),
                    parity,
                    &[],
                    None,
                    &[]
                ),
                Err(PlanError::ParityWithHybridRows)
            );
        }
    }

    #[test]
    fn a_hybrid_site_is_not_subjected_to_the_uniform_height_multiple_rule() {
        // R10 divides heights; a hybrid's compatibility is decided by pattern matching instead.
        // Applying both would reject a legal hybrid whose height is not a multiple of the base's.
        let (base, ..) = hybrid_pair();
        let odd = Site {
            name: "Odd".into(),
            width: 50,
            height: 400,
            row_pattern: base.row_pattern.clone(),
        };
        assert!(plan(
            Rect::new(0, 0, 10_000, 10_000),
            Rect::new(0, 0, 5_000, 4_000),
            &[base, odd],
            RowParity::None,
            &[],
            None,
            &[]
        )
        .is_ok());
    }

    #[test]
    fn a_master_taller_than_the_core_is_refused_and_named() {
        // The reference failure: init_floorplan10's macro is 59.09 x 65.80 um in a 60 x 60 core,
        // so it fits horizontally and not vertically. Checking only one axis would pass it.
        let core = Rect::new(20_000, 20_000, 80_000, 80_000);
        let macro_ = inst("dcache.mem", 59_090, 65_800);
        assert_eq!(
            check_instance_dimensions(&[macro_], core),
            Err(PlanError::InstanceDoesNotFit {
                name: "dcache.mem".into(),
                width: 59_090,
                height: 65_800,
                core,
            })
        );
        // Exactly the size of the core still fits: the comparison is >, not >=.
        assert!(check_instance_dimensions(&[inst("snug", 60_000, 60_000)], core).is_ok());
    }

    #[test]
    fn pads_and_covers_are_exempt_but_ordinary_masters_are_not() {
        let core = Rect::new(0, 0, 1_000, 1_000);
        let huge = inst("huge", 9_999, 9_999);
        assert!(check_instance_dimensions(std::slice::from_ref(&huge), core).is_err());
        for exempt in [
            Instance {
                is_pad: true,
                ..huge.clone()
            },
            Instance {
                is_cover: true,
                ..huge.clone()
            },
        ] {
            assert!(
                check_instance_dimensions(&[exempt], core).is_ok(),
                "a pad or cover belongs outside the core and is not measured against it"
            );
        }
    }

    #[test]
    fn an_r90_symmetric_master_only_has_to_fit_the_larger_dimension() {
        // A tall, narrow core and a wide, short master: it does not fit as drawn, but a master
        // free to rotate does fit. Comparing width-to-width would wrongly refuse it.
        let core = Rect::new(0, 0, 1_000, 5_000);
        let wide = inst("wide", 4_000, 500);
        assert!(check_instance_dimensions(std::slice::from_ref(&wide), core).is_err());
        assert!(check_instance_dimensions(
            &[Instance {
                symmetry_r90: true,
                ..wide
            }],
            core
        )
        .is_ok());
        // Rotation does not make something bigger than BOTH dimensions fit.
        assert!(check_instance_dimensions(
            &[Instance {
                symmetry_r90: true,
                ..inst("giant", 6_000, 6_000)
            }],
            core
        )
        .is_err());
    }

    #[test]
    fn the_design_area_counts_every_instance_including_the_exempt_ones() {
        // R12 is a census of the design; R13 is a question about the core. They deliberately
        // disagree about pads -- the area includes what the fit check skips.
        let insts = vec![
            inst("a", 100, 200),
            Instance {
                is_pad: true,
                ..inst("p", 300, 400)
            },
        ];
        assert_eq!(design_area(&insts), 100.0 * 200.0 + 300.0 * 400.0);
        assert_eq!(design_area(&[]), 0.0);
    }

    #[test]
    fn the_die_error_wins_over_an_oversized_macro() {
        // Upstream checks the die first. A design with both problems must report the one the
        // caller can act on, and the order is part of the contract, not an accident.
        let huge = inst("huge", 9_999_999, 9_999_999);
        assert_eq!(
            plan(
                Rect::new(0, 0, 0, 0),
                Rect::new(0, 0, 0, 0),
                &[nangate_site()],
                RowParity::None,
                &[],
                None,
                &[huge]
            ),
            Err(PlanError::EmptyDieArea)
        );
    }

    #[test]
    fn the_refusals_are_specific() {
        let site = nangate_site();
        let die = Rect::new(0, 0, 1_000_000, 1_000_000);
        assert_eq!(
            plan(
                Rect::new(0, 0, 0, 0),
                die,
                std::slice::from_ref(&site),
                RowParity::None,
                &[],
                None,
                &[]
            ),
            Err(PlanError::EmptyDieArea)
        );
        assert_eq!(
            plan(
                die,
                Rect::new(-10, -10, 2_000_000, 2_000_000),
                std::slice::from_ref(&site),
                RowParity::None,
                &[],
                None,
                &[]
            ),
            Err(PlanError::CoreNotInDie)
        );
        // A core smaller than one site tiles nothing — an error, not an empty floorplan.
        assert_eq!(
            plan(
                die,
                Rect::new(0, 0, 10, 10),
                std::slice::from_ref(&site),
                RowParity::None,
                &[],
                None,
                &[]
            ),
            Err(PlanError::NoRows)
        );
        let odd = Site::plain("odd", 380, 2800 + 1);
        assert!(matches!(
            plan(
                die,
                Rect::new(0, 0, 500_000, 500_000),
                &[site, odd],
                RowParity::None,
                &[],
                None,
                &[]
            ),
            Err(PlanError::SiteHeightNotMultiple { .. })
        ));
    }
}

// ------------------------------------------------------------ voltage domains

/// A voltage or power domain: its group name, and the bounding box of its region's boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    pub name: String,
    pub bbox: Rect,
}

/// Upstream `odb::makeSiteLoc` (`odb/src/db/util.cpp`) — snap a coordinate to the site grid.
///
/// ```text
/// site_x  = (x - offset) / site_width          // site_width is a DOUBLE upstream
/// site_x1 = at_left_from_macro ? floor(site_x) : ceil(site_x)
/// return    site_x1 * site_width + offset      // int * double -> double, TRUNCATED on return
/// ```
///
/// ⚠️ **`at_left_from_macro` selects `floor`, not `ceil`** — the name reads as a side and behaves
/// as a direction, and `updateVoltageDomain` passes `false` for a minimum and `true` for a
/// maximum, which is how it snaps a domain box *inward*.
///
/// ⚠️ **The arithmetic is in `double` and truncates on the way back to `int`.** Doing it in
/// integers instead would round the other way for a coordinate that is already on the grid but
/// negative.
pub fn make_site_loc(x: i32, site_width: i32, at_left_from_macro: bool, offset: i32) -> i32 {
    let site_x = (x - offset) as f64 / site_width as f64;
    let site_x1 = (if at_left_from_macro { site_x.floor() } else { site_x.ceil() }) as i32;
    (site_x1 as f64 * site_width as f64 + offset as f64) as i32
}

/// Upstream `InitFloorplan::updateVoltageDomain` — rebuild the rows that cross a domain.
///
/// 🔑 **The whole rule** (`ifp/src/InitFloorplan.cc:539`), per domain group:
///
/// ```text
/// rows            = every row whose site class is NOT PAD
/// min_site_dx/dy  = the smallest site width/height among those rows
/// space           = gap, or 6 * min_site_dy when no gap was given
/// domain box      = the region's bbox, snapped INWARD to the site grid
/// for each row:
///     if row_y_max + space <= domain_y_min  or  row_y_min >= domain_y_max + space:  keep it
///     else: DESTROY it and create, in this order,
///         <row>_1        left of the domain,  if domain_x_min - space > core_lx + site_dx
///         <row>_2        right of the domain, if snapped(domain_x_max + space) + site_dx < core_ux
///         <row>_<domain> across the domain,   if the row lies wholly within its y range
/// ```
///
/// ⚠️ **`space` is named `power_domain_y_space` upstream and is used on BOTH axes** — the left and
/// right margins are the same value as the vertical one. Not a transcription slip; do not "fix" it.
///
/// ⚠️ **Only the right edge is re-snapped.** `rcr_x_min` goes through `makeSiteLoc` again;
/// `lcr_x_max` does not.
///
/// 🔑 **Destroyed rows are removed and their pieces APPENDED**, so the resulting order is every
/// surviving row in its original order followed by every piece in creation order. That is what
/// this returns, and it is why the whole set can be rebuilt in one pass instead of destroying
/// rows one at a time.
///
/// Domains are applied in sequence, each seeing the rows the previous one left, because upstream
/// re-reads `block_->getRows()` inside its group loop.
pub fn split_rows_for_domains(
    rows: Vec<Row>,
    domains: &[Domain],
    core: Rect,
    gap: Option<i32>,
    site_height: &dyn Fn(&str) -> i32,
    site_is_pad: &dyn Fn(&str) -> bool,
) -> Vec<Row> {
    let mut rows = rows;
    for d in domains {
        rows = split_rows_for_one_domain(rows, d, core, gap, site_height, site_is_pad);
    }
    rows
}

fn split_rows_for_one_domain(
    rows: Vec<Row>,
    domain: &Domain,
    core: Rect,
    gap: Option<i32>,
    site_height: &dyn Fn(&str) -> i32,
    site_is_pad: &dyn Fn(&str) -> bool,
) -> Vec<Row> {
    // Upstream builds its working list from the non-PAD rows and returns early when it is empty.
    // A PAD row is never destroyed and never inspected; it simply stays where it is.
    let considered: Vec<&Row> = rows.iter().filter(|r| !site_is_pad(&r.site)).collect();
    if considered.is_empty() {
        return rows;
    }
    let min_site_dx = considered.iter().map(|r| r.spacing).min().unwrap();
    let min_site_dy = considered.iter().map(|r| site_height(&r.site)).min().unwrap();
    if min_site_dx <= 0 || min_site_dy <= 0 {
        return rows;
    }
    let space = gap.unwrap_or(6 * min_site_dy);

    // Inward: the minimum ceils and the maximum floors.
    let dx_min = make_site_loc(domain.bbox.x_min, min_site_dx, false, 0);
    let dx_max = make_site_loc(domain.bbox.x_max, min_site_dx, true, 0);
    let dy_min = make_site_loc(domain.bbox.y_min, min_site_dy, false, 0);
    let dy_max = make_site_loc(domain.bbox.y_max, min_site_dy, true, 0);

    // 🔑 **Where a piece LANDS is decided by OpenDB, not by `ifp`.** `dbRow::destroy` frees the
    // row's table slot and the next `dbRow::create` reuses it, so a destroyed row's FIRST piece
    // takes its place in the row list and every later piece is appended. Reproduced because the
    // DEF is written in that order and the goldens assert it — ⛔ but it is a property of the
    // database, not a rule of the algorithm, so do not read intent into it.
    //
    // Measured 2026-09-01 on `init_floorplan_dbl_row`, the case that can tell the two apart:
    // `ROW_19_1`..`ROW_52_1` sit in the destroyed rows' own positions while their `_2` and
    // `_TEMP_ANALOG` pieces are appended, interleaved per row in creation order. `init_floorplan8`
    // produces no `_1` piece at all and cannot distinguish "the first piece" from "the `_2` piece".
    let mut in_place: Vec<Row> = Vec::new();
    let mut appended: Vec<Row> = Vec::new();
    for row in &rows {
        if site_is_pad(&row.site) {
            in_place.push(row.clone());
            continue;
        }
        let y_min = row.y;
        let y_max = row.y + site_height(&row.site);
        let site_dx = row.spacing;
        if y_max + space <= dy_min || y_min >= dy_max + space {
            in_place.push(row.clone());
            continue;
        }

        // Built in upstream's creation order: left, right, then the domain row.
        let mut pieces: Vec<Row> = Vec::new();
        let lcr_x_max = dx_min - space;
        if lcr_x_max > core.x_min + site_dx {
            pieces.push(Row {
                name: format!("{}_1", row.name),
                num_sites: (lcr_x_max - core.x_min) / site_dx,
                x: core.x_min,
                ..row.clone()
            });
        }

        let rcr_x_min = make_site_loc(dx_max + space, site_dx, false, 0);
        if rcr_x_min + site_dx < core.x_max {
            pieces.push(Row {
                name: format!("{}_2", row.name),
                num_sites: (core.x_max - rcr_x_min) / site_dx,
                x: rcr_x_min,
                ..row.clone()
            });
        }

        // The domain row itself, only where the row lies WHOLLY inside the domain's y range —
        // the rows in the margin above and below lose their middle and get no replacement.
        if y_min >= dy_min && y_max <= dy_max {
            pieces.push(Row {
                name: format!("{}_{}", row.name, domain.name),
                num_sites: (dx_max - dx_min) / site_dx,
                x: dx_min,
                ..row.clone()
            });
        }

        // ⬜ **A destroyed row with NO pieces leaves a HOLE**, and which later create fills it
        // depends on odb's free-list discipline. No case in the corpus reaches that — every
        // destroyed row in all three domain cases yields at least one piece — so it is left
        // unmodelled rather than guessed at. See the divergence register.
        let mut it = pieces.into_iter();
        if let Some(first) = it.next() {
            in_place.push(first);
        }
        appended.extend(it);
    }
    in_place.extend(appended);
    in_place
}

#[cfg(test)]
mod voltage_domain_tests {
    use super::*;

    fn row(name: &str, y: i32, site: &str) -> Row {
        Row {
            name: name.into(),
            site: site.into(),
            x: 0,
            y,
            orient: "N".into(),
            num_sites: 100,
            spacing: 10,
        }
    }

    /// ⚠️ `at_left_from_macro` is `floor`, and the arithmetic goes through `double`.
    #[test]
    fn make_site_loc_floors_for_a_maximum_and_ceils_for_a_minimum() {
        // 27 on a 10-wide grid: inward from below is 30, inward from above is 20.
        assert_eq!(make_site_loc(27, 10, false, 0), 30, "ceil");
        assert_eq!(make_site_loc(27, 10, true, 0), 20, "floor");
        // Already on the grid: neither moves it.
        assert_eq!(make_site_loc(30, 10, false, 0), 30);
        assert_eq!(make_site_loc(30, 10, true, 0), 30);
        // The offset is subtracted before and added back after.
        assert_eq!(make_site_loc(27, 10, true, 5), 25);
    }

    /// ⛔ A row clear of the domain and its margin is untouched; a row crossing it is REPLACED by
    /// its pieces, and the pieces are appended after every survivor.
    #[test]
    fn a_row_crossing_a_domain_is_replaced_and_the_first_piece_takes_its_place() {
        let h = |_: &str| 10;
        let pad = |_: &str| false;
        // Core 0..1000 x 0..1000, domain 300..600 in both axes, gap 0 so the margin is exactly
        // the domain box and only the crossing rows move.
        let d = Domain {
            name: "PD".into(),
            bbox: Rect { x_min: 300, y_min: 300, x_max: 600, y_max: 600 },
        };
        let core = Rect { x_min: 0, y_min: 0, x_max: 1000, y_max: 1000 };
        let rows = vec![row("ROW_0", 0, "s"), row("ROW_1", 400, "s"), row("ROW_2", 900, "s")];

        let out = split_rows_for_domains(rows, &[d], core, Some(0), &h, &pad);
        let names: Vec<&str> = out.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ROW_0", "ROW_1_1", "ROW_2", "ROW_1_2", "ROW_1_PD"],
            "the FIRST piece takes the destroyed row's place; the rest are appended"
        );

        let left = out.iter().find(|r| r.name == "ROW_1_1").unwrap();
        assert_eq!((left.x, left.num_sites), (0, 30), "core_lx .. domain_x_min - gap");
        let right = out.iter().find(|r| r.name == "ROW_1_2").unwrap();
        assert_eq!((right.x, right.num_sites), (600, 40), "domain_x_max .. core_ux");
        let mid = out.iter().find(|r| r.name == "ROW_1_PD").unwrap();
        assert_eq!((mid.x, mid.num_sites), (300, 30), "across the domain itself");
    }

    /// ⛔ **The default margin is 6x the minimum site height, not zero** — so rows well clear of
    /// the domain box are still rebuilt.
    #[test]
    fn the_default_margin_is_six_minimum_site_heights() {
        let h = |_: &str| 10;
        let pad = |_: &str| false;
        let d = Domain {
            name: "PD".into(),
            bbox: Rect { x_min: 300, y_min: 300, x_max: 600, y_max: 600 },
        };
        let core = Rect { x_min: 0, y_min: 0, x_max: 1000, y_max: 1000 };
        // The margin is 6 * 10 = 60, and the keep test is `y_max + space <= domain_y_min`.
        //
        // y=230 -> y_max 240, and 240 + 60 == 300 exactly, so the row is KEPT. The comparison is
        // `<=`, and a row touching the margin edge survives.
        let kept = split_rows_for_domains(vec![row("R", 230, "s")], &[d.clone()], core, None, &h, &pad);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "R", "exactly on the margin edge, so untouched");

        // y=240 -> 250 + 60 > 300, one site height further in, and now it is rebuilt.
        let out = split_rows_for_domains(vec![row("R", 240, "s")], &[d], core, None, &h, &pad);
        assert!(out.iter().all(|r| r.name != "R"), "the row itself is gone");
        let names: Vec<&str> = out.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["R_1", "R_2"], "_1 in place, _2 appended");
        // ⚠️ It sits in the MARGIN, below the domain's own y range, so it gets NO domain row --
        // the middle is simply lost.
        assert!(out.iter().all(|r| r.name != "R_PD"), "margin rows lose their middle entirely");
    }

    /// A PAD row is never inspected and never destroyed — upstream leaves it out of the working
    /// list altogether.
    #[test]
    fn pad_rows_are_left_alone() {
        let h = |_: &str| 10;
        let d = Domain {
            name: "PD".into(),
            bbox: Rect { x_min: 300, y_min: 300, x_max: 600, y_max: 600 },
        };
        let core = Rect { x_min: 0, y_min: 0, x_max: 1000, y_max: 1000 };
        let out = split_rows_for_domains(
            vec![row("PADROW", 400, "padsite")],
            &[d],
            core,
            Some(0),
            &h,
            &|s| s == "padsite",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "PADROW", "a PAD row crossing the domain still survives");
    }
}

// ---------------------------------------------------------------- make_tracks

/// One layer's track pattern on one axis, exactly as `dbTrackGrid::addGridPattern*` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackPattern {
    pub origin: i32,
    pub count: i32,
    pub step: i32,
}

/// Upstream `InitFloorplan::makeTracks(layer, x_offset, x_pitch, y_offset, y_pitch)`, one axis.
///
/// 🔑 **The whole rule, transcribed** (`ifp/src/InitFloorplan.cc:1032`):
///
/// ```text
/// if offset == 0        -> offset = pitch          // a zero offset means "one pitch in"
/// if offset > die span  -> SKIP the layer entirely (IFP-21 / IFP-22)
/// count  = (die_span - offset) / pitch + 1         // INTEGER division
/// origin = die_min + offset
/// if origin - min_width/2 < die_min  -> origin += pitch; count--   // first track unroutable
/// last   = origin + (count - 1) * pitch
/// if last + min_width/2 > die_max    -> count--                    // last track unroutable
/// ```
///
/// ⚠️ **`min_width / 2` is INTEGER division**, and both guards use it. An odd `min_width` therefore
/// rounds toward zero — writing this as a float halves a DBU and moves the first track on any layer
/// whose min width is odd.
///
/// ⚠️ **The two guards are SEQUENTIAL, not alternatives.** The last-track check reads the origin the
/// first-track check may already have moved, so a pattern can lose a track at each end.
///
/// Returns `None` when upstream skips the layer, which is a WARNING there and must not be
/// silently turned into an empty pattern — an empty grid compares equal to every other empty grid.
pub fn track_pattern(
    die_min: i32,
    die_span: i32,
    offset: i32,
    pitch: i32,
    min_width: i32,
) -> Option<TrackPattern> {
    let offset = if offset == 0 { pitch } else { offset };
    if offset > die_span {
        return None;
    }
    let mut count = (die_span - offset) / pitch + 1;
    let mut origin = die_min + offset;
    if origin - min_width / 2 < die_min {
        origin += pitch;
        count -= 1;
    }
    let last = origin + (count - 1) * pitch;
    if last + min_width / 2 > die_min + die_span {
        count -= 1;
    }
    Some(TrackPattern { origin, count, step: pitch })
}

/// Which axis' offset overflowed the die. Upstream returns on the FIRST one, so only one is ever
/// reported even when both are out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSkip {
    /// `x_offset > die width` — IFP-21.
    X,
    /// `y_offset > die height` — IFP-22.
    Y,
}

/// Upstream `InitFloorplan::makeTracks(layer, ...)` — the WHOLE function, both axes together.
///
/// ⛔ **The skip is the whole LAYER, not one axis.** Upstream tests `x_offset`, then `y_offset`,
/// and `return`s from `makeTracks` on either — and the `return` sits *above* `findTrackGrid`, so
/// neither `addGridPatternX` nor `addGridPatternY` runs. An out-of-range **y** offset therefore
/// suppresses the **x** pattern too, even though x passed its own test.
///
/// 🔑 **This is why the axes cannot be planned independently.** Calling [`track_pattern`] once per
/// axis and skipping only the failing one is a faithful transcription of the body placed in the
/// wrong call structure — every warning is still correct, so no log comparison can see it.
///
/// Measured 2026-09-01 at pin `945a9f4` on upstream's own `make_tracks4`, the case written for
/// this rule: the reference writes **zero** TRACKS for its two calls; the per-axis form wrote
/// **two** patterns (a Y for the call whose x overflowed, an X for the call whose y overflowed),
/// each with the reference's own numbers. The control — the same layer with both offsets in
/// range — agrees exactly on both sides, which is what makes the difference attributable.
pub fn track_patterns(
    die_x: i32,
    dx: i32,
    die_y: i32,
    dy: i32,
    x_offset: i32,
    x_pitch: i32,
    y_offset: i32,
    y_pitch: i32,
    min_width: i32,
) -> Result<(TrackPattern, TrackPattern), TrackSkip> {
    // `?` reproduces the early return: X is tested first, and a failure on either axis leaves the
    // caller with nothing to add.
    let px = track_pattern(die_x, dx, x_offset, x_pitch, min_width).ok_or(TrackSkip::X)?;
    let py = track_pattern(die_y, dy, y_offset, y_pitch, min_width).ok_or(TrackSkip::Y)?;
    Ok((px, py))
}

/// Upstream `InitFloorplan::makeTracksNonUniform` — the SEQUENCE of ordinary `makeTracks` calls a
/// layer carrying `LEF58_PITCH` (`FIRSTLASTPITCH`) expands into. Returns each call's `y_offset`.
///
/// 🔑 **The whole rule, transcribed** (`ifp/src/InitFloorplan.cc:1122`):
///
/// ```text
/// cell_row_height = the height of the FIRST row whose site class is CORE    // IFP-45 if none
/// y_track_count   = (cell_row_height - 2*first_last_pitch) / y_pitch + 1
/// origin_y        = die_y_min + first_last_pitch
/// repeat y_track_count times:
///     makeTracks(layer, x_offset, x_pitch, origin_y, cell_row_height)
///     origin_y += y_pitch
/// origin_y += first_last_pitch - y_pitch
/// makeTracks(layer, x_offset, x_pitch, origin_y, cell_row_height)           // one MORE, always
/// ```
///
/// ⚠️ **Every call passes `cell_row_height` as the y PITCH**, not `y_pitch`. The row height is what
/// the pattern repeats on; `y_pitch` only spaces the origins *within* one row. Reading `y_pitch` as
/// the pattern step produces one grid at the wrong spacing instead of a stack of them.
///
/// ⚠️ **The layer's own `y_offset` is passed to the reference and never read.** Its body derives
/// every origin from `first_last_pitch`, so the parameter is dead — do not "restore" it.
///
/// ⚠️ **The final call is unconditional**, so the layer gets `y_track_count + 1` patterns on each
/// axis, and the x pattern is therefore added **identically that many times**. `y_track_count` can
/// be zero or negative when `first_last_pitch` is large, leaving only that final call. Reproduced
/// without a guard, because upstream has none.
///
/// Measured 2026-09-01 on ASAP7 (`make_tracks7`): `cell_row_height` 270, `y_pitch` 36,
/// `first_last_pitch` 45 gives origins **45, 81, 117, 153, 189, 225, 270** — the reference's own
/// seven `TRACKS Y … STEP 270` lines, the last with one fewer track because it starts higher.
pub fn non_uniform_track_origins(
    die_y_min: i32,
    cell_row_height: i32,
    y_pitch: i32,
    first_last_pitch: i32,
) -> Vec<i32> {
    let y_track_count = (cell_row_height - 2 * first_last_pitch) / y_pitch + 1;
    let mut origin_y = die_y_min + first_last_pitch;
    let mut out = Vec::new();
    for _ in 0..y_track_count {
        out.push(origin_y);
        origin_y += y_pitch;
    }
    origin_y += first_last_pitch - y_pitch;
    out.push(origin_y);
    out
}

/// Upstream `ifp::microns_to_mfg_grid` (`InitFloorplan.tcl:295`).
///
/// 🔑 **Double rounding, and it is not decoration**: `round(round(um * dbu / grid) * grid)`. The
/// inner round picks the nearest whole manufacturing-grid step; the outer one lands it on a DBU.
/// Collapsing this to `round(um * dbu)` puts an off-grid track origin into the database.
pub fn microns_to_mfg_grid(microns: f64, dbu_per_micron: i32, manufacturing_grid: i32) -> i32 {
    if manufacturing_grid > 0 {
        let g = manufacturing_grid as f64;
        ((microns * dbu_per_micron as f64 / g).round() * g).round() as i32
    } else {
        (microns * dbu_per_micron as f64).round() as i32
    }
}

#[cfg(test)]
mod make_tracks_tests {
    use super::*;

    /// The plain case: no guard fires, so the arithmetic alone decides.
    #[test]
    fn a_pattern_starts_one_offset_in_and_counts_whole_pitches() {
        // die 0..1000, offset 100, pitch 100, min_width 0:
        //   count = (1000-100)/100 + 1 = 10, origin = 100, last = 100+9*100 = 1000
        //   first guard: 100 - 0 < 0 ? no.   last guard: 1000 + 0 > 1000 ? no.
        let p = track_pattern(0, 1000, 100, 100, 0).unwrap();
        assert_eq!(p, TrackPattern { origin: 100, count: 10, step: 100 });
    }

    /// ⚠️ Upstream reads a ZERO offset as "one pitch in", not as "at the die edge".
    #[test]
    fn a_zero_offset_becomes_one_pitch() {
        assert_eq!(track_pattern(0, 1000, 0, 100, 0), track_pattern(0, 1000, 100, 100, 0));
    }

    /// IFP-21/22: an offset past the die is skipped, not clamped.
    #[test]
    fn an_offset_wider_than_the_die_skips_the_layer() {
        assert_eq!(track_pattern(0, 500, 600, 100, 0), None, "upstream warns and returns");
    }

    /// ⛔ **The skip is the whole LAYER.** `makeTracks` `return`s above `findTrackGrid`, so an
    /// out-of-range offset on EITHER axis leaves the layer with no grid at all — the other axis
    /// is not created, even when it passed its own test.
    ///
    /// Measured 2026-09-01 on upstream's `make_tracks4` (die 200x200 um, both calls on metal2):
    /// the reference wrote **zero** TRACKS; planning the axes independently wrote two patterns.
    #[test]
    fn one_axis_over_the_die_suppresses_the_other_axis_too() {
        // x overflows, y is perfectly legal on its own — upstream still creates nothing.
        assert!(track_pattern(0, 1000, 100, 100, 0).is_some(), "y alone would be fine");
        assert_eq!(track_patterns(0, 500, 0, 1000, 600, 100, 100, 100, 0), Err(TrackSkip::X));

        // y overflows AFTER x has passed: upstream has computed nothing and returns.
        assert!(track_pattern(0, 1000, 100, 100, 0).is_some(), "x alone would be fine");
        assert_eq!(track_patterns(0, 1000, 0, 500, 100, 100, 600, 100, 0), Err(TrackSkip::Y));

        // X is tested first, so a layer failing both is reported as X and never as Y.
        assert_eq!(track_patterns(0, 500, 0, 500, 600, 100, 600, 100, 0), Err(TrackSkip::X));

        // Both in range: the pair is created, and each axis matches the per-axis rule exactly.
        assert_eq!(
            track_patterns(0, 1000, 0, 1000, 100, 100, 100, 100, 0),
            Ok((
                track_pattern(0, 1000, 100, 100, 0).unwrap(),
                track_pattern(0, 1000, 100, 100, 0).unwrap()
            ))
        );
    }

    /// ⛔ A layer carrying `LEF58_PITCH` is a STACK of patterns, one per track within the cell row,
    /// each repeating on the ROW HEIGHT — not one grid at the layer's own pitch.
    ///
    /// ASAP7's M2 (`PITCH 0.036 FIRSTLASTPITCH 0.045`, 1000 DBU/um, 270-DBU core row) is the
    /// reference's own witness, and `make_tracks7` is the case that carries it.
    #[test]
    fn a_first_last_pitch_layer_expands_into_a_stack_of_row_height_patterns() {
        assert_eq!(
            non_uniform_track_origins(0, 270, 36, 45),
            vec![45, 81, 117, 153, 189, 225, 270],
            "six inside the row, then one more from first_last_pitch - y_pitch"
        );
        // 261 + 45 - 36 = 270: the tail is NOT another y_pitch step.
        assert_eq!(*non_uniform_track_origins(0, 270, 36, 45).last().unwrap(), 270);

        // The die origin is carried into every call, exactly as `die_area.yMin() +` does.
        assert_eq!(non_uniform_track_origins(1000, 270, 36, 45)[0], 1045);

        // ⚠️ first_last_pitch large enough to make the count non-positive leaves ONLY the final
        // call — upstream has no guard, so neither do we. (100 - 2*70) / 36 = -40/36 = -1
        // TRUNCATING TOWARD ZERO, as C++ does since C++11, so the count is 0 rather than -1+1.
        assert_eq!(non_uniform_track_origins(0, 100, 36, 70), vec![104]);
    }

    /// The first-track guard moves the origin AND drops a track.
    #[test]
    fn an_unroutable_first_track_is_dropped_and_the_origin_moves() {
        // offset 100, min_width 400 -> 100 - 200 = -100 < 0, so origin -> 200 and count -> 9.
        // then last = 200 + 8*100 = 1000, and 1000 + 200 > 1000, so the last goes too: count 8.
        let p = track_pattern(0, 1000, 100, 100, 400).unwrap();
        assert_eq!(p, TrackPattern { origin: 200, count: 8, step: 100 });
    }

    /// ⛔ `min_width / 2` is INTEGER division. With min_width 201 the half is 100, not 100.5, so
    /// the guard does NOT fire at origin 100 — a float would make it fire and move every track.
    #[test]
    fn the_half_min_width_truncates_rather_than_rounding() {
        let p = track_pattern(0, 1000, 100, 100, 201).unwrap();
        assert_eq!(p.origin, 100, "100 - 201/2 = 100 - 100 = 0, which is not < 0");
        let q = track_pattern(0, 1000, 100, 100, 202).unwrap();
        assert_eq!(q.origin, 200, "100 - 101 = -1 < 0, so this one does move");
    }

    /// The die does not have to start at zero, and every comparison is against its own bounds.
    #[test]
    fn the_die_origin_is_carried_into_the_track_origin() {
        let p = track_pattern(500, 1000, 100, 100, 0).unwrap();
        assert_eq!(p.origin, 600, "die_min + offset");
        assert_eq!(p.count, 10);
    }

    /// Upstream's own Nangate45 numbers, so the transcription is pinned to a real technology.
    #[test]
    fn nangate45_metal1_matches_the_technologys_own_track_file() {
        // Nangate45_tech.lef: DATABASE MICRONS 2000, MANUFACTURINGGRID 0.0050 -> 10 DBU.
        // Nangate45.tracks: metal1 -x_offset 0.095 -x_pitch 0.19 -y_offset 0.07.
        assert_eq!(microns_to_mfg_grid(0.095, 2000, 10), 190);
        assert_eq!(microns_to_mfg_grid(0.19, 2000, 10), 380);
        assert_eq!(microns_to_mfg_grid(0.07, 2000, 10), 140);
    }

    /// ⛔ The double rounding is load-bearing: a value between grid steps snaps to the grid first.
    #[test]
    fn microns_snap_to_the_manufacturing_grid_before_becoming_dbu() {
        // 0.0101 um at 1000 DBU/um is 10.1 DBU; on a 5-DBU grid that is 2.02 steps -> 2 -> 10.
        assert_eq!(microns_to_mfg_grid(0.0101, 1000, 5), 10);
        // With no manufacturing grid the value is simply rounded to DBU.
        assert_eq!(microns_to_mfg_grid(0.0101, 1000, 0), 10);
        assert_eq!(microns_to_mfg_grid(0.0106, 1000, 0), 11, "no grid: plain rounding");
    }
}
