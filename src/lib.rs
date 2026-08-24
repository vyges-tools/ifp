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
