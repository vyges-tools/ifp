// SPDX-License-Identifier: Apache-2.0
//! End-to-end: drive the real binary, then read the database back with an independent reader.
//!
//! The unit tests prove the arithmetic; these prove the arithmetic actually *lands* — that the
//! rows exist in the written `.odb` and that the core area odb reports is the one the report
//! claimed. A planner that is right about a database it never wrote would pass every unit test
//! in the crate.

use std::path::PathBuf;
use std::process::Command;
use vyges_opendb::Db;

/// Borrowed from the sibling crate rather than duplicated: 1.5 MB of binary is not worth a copy.
const FIXTURE: &str = "../vyges-tools-opendb-lib/test/fixtures/counter.odb";
const BIN: &str = env!("CARGO_BIN_EXE_vyges-ifp");

/// sky130hd's `unithd`, which the fixture's library defines. Discovered once, asserted below so
/// a fixture swap fails loudly instead of quietly changing what these numbers mean.
const SITE: &str = "unithd";
const SITE_W: i32 = 460;
const SITE_H: i32 = 2720;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vyges-ifp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir.join(name)
}

fn fixture_copy(name: &str) -> PathBuf {
    let p = scratch(name);
    std::fs::copy(FIXTURE, &p).expect("the sibling fixture is readable");
    p
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("the binary runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

/// The report's value for `key`, as an integer. Deliberately crude — this is a smoke check of
/// the envelope, not a JSON library.
fn field(report: &str, key: &str) -> i64 {
    let at = report
        .find(&format!("\"{key}\""))
        .unwrap_or_else(|| panic!("no {key} in {report}"));
    report[at..]
        .split(':')
        .nth(1)
        .and_then(|t| t.trim().trim_end_matches(',').split(['\n', ',']).next())
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or_else(|| panic!("{key} is not an integer in {report}"))
}

#[test]
fn the_site_the_numbers_here_assume_is_the_one_the_fixture_defines() {
    let db = Db::open(FIXTURE).expect("fixture opens");
    assert_eq!(
        db.dbu_per_micron(),
        1000,
        "these tests state microns at 1000 DBU/um"
    );
    assert_eq!(
        (db.site_get_width(SITE), db.site_get_height(SITE)),
        (SITE_W, SITE_H),
        "the fixture's {SITE} site changed; the expected row counts below are stale"
    );
}

#[test]
fn a_floorplan_survives_the_write_and_the_read_back() {
    let input = fixture_copy("in.odb");
    let out = scratch("out.odb");
    let (code, report) = run(&[
        "run",
        input.to_str().unwrap(),
        "--die-area",
        "0 0 100 100",
        "--core-area",
        "10 10 90 90",
        "--site",
        SITE,
        "--out-odb",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "applied: {report}");

    // Independently derived, not copied from the run: the lower left snaps UP a whole site,
    // and the counts are what fits between there and the untouched upper right.
    let ceil_to = |v: i32, m: i32| ((v + m - 1) / m) * m;
    let (x0, y0) = (ceil_to(10_000, SITE_W), ceil_to(10_000, SITE_H));
    let (nx, ny) = ((90_000 - x0) / SITE_W, (90_000 - y0) / SITE_H);
    assert_eq!((x0, y0, nx, ny), (10_120, 10_880, 173, 29));
    assert_eq!(field(&report, "rows"), ny as i64);
    assert_eq!(field(&report, "sites_per_row"), nx as i64);

    // The database is the claim that matters.
    let db = Db::open(&out).expect("the written database opens");
    assert_eq!(
        db.num_rows().expect("rows"),
        ny as usize,
        "the rows are really there"
    );
    let core = db.compute_core_area().expect("core");
    assert_eq!(core, vec![x0, y0, x0 + nx * SITE_W, y0 + ny * SITE_H]);
    let die = (
        db.block_get_die_area_x_min(),
        db.block_get_die_area_y_min(),
        db.block_get_die_area_x_max(),
        db.block_get_die_area_y_max(),
    );
    assert_eq!(
        die,
        (0, 0, 100_000, 100_000),
        "the die is what was asked for, unsnapped"
    );

    // ...and the input was left alone, because --out-odb was given. Compared against the
    // untouched fixture rather than against `ny`, which the input could match by chance.
    let pristine = Db::open(FIXTURE)
        .expect("fixture opens")
        .num_rows()
        .expect("rows");
    let after = Db::open(&input)
        .expect("the input still opens")
        .num_rows()
        .expect("rows");
    assert_eq!(after, pristine, "--out-odb must not write in place");
}

#[test]
fn a_dry_run_reports_the_same_plan_and_writes_nothing() {
    let input = fixture_copy("dry.odb");
    let before = std::fs::metadata(&input).expect("stat").len();
    let (code, report) = run(&[
        "run",
        input.to_str().unwrap(),
        "--die-area",
        "0 0 100 100",
        "--core-area",
        "10 10 90 90",
        "--site",
        SITE,
        "--dry-run",
    ]);
    assert_eq!(code, 0);
    assert!(report.contains("\"status\": \"planned\""), "{report}");
    assert!(report.contains("\"odb_written\": null"), "{report}");
    assert_eq!(
        field(&report, "rows"),
        29,
        "the plan is the same one the applied run made"
    );
    assert_eq!(
        std::fs::metadata(&input).expect("stat").len(),
        before,
        "untouched"
    );
}

#[test]
fn rows_are_rebuilt_rather_than_appended_when_run_twice() {
    // A flow re-runs the floorplan after changing the die. If rows accumulated, the second run
    // would leave the first run's grid behind and the core area would be wrong.
    let input = fixture_copy("twice.odb");
    let args = |die: &str, core: &str| {
        vec![
            "run".to_string(),
            input.to_str().unwrap().to_string(),
            "--die-area".into(),
            die.into(),
            "--core-area".into(),
            core.into(),
            "--site".into(),
            SITE.into(),
        ]
    };
    let first = args("0 0 100 100", "10 10 90 90");
    let (c1, _) = run(&first.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(c1, 0);
    let second = args("0 0 60 60", "10 10 50 50");
    let (c2, r2) = run(&second.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(c2, 0, "{r2}");

    let db = Db::open(&input).expect("opens");
    let expect = (50_000 - 10_880) / SITE_H;
    assert_eq!(
        db.num_rows().expect("rows") as i32,
        expect,
        "the second run replaced the first"
    );
    assert_eq!(field(&r2, "rows"), expect as i64);
}

#[test]
fn a_design_that_cannot_be_floorplanned_is_refused_without_being_touched() {
    let input = fixture_copy("refused.odb");
    let before = std::fs::metadata(&input).expect("stat").len();
    // The core sticks out of the die.
    let (code, _) = run(&[
        "run",
        input.to_str().unwrap(),
        "--die-area",
        "0 0 100 100",
        "--core-area",
        "10 10 900 900",
        "--site",
        SITE,
    ]);
    assert_eq!(code, 1, "a refusal is a verdict (1), not an error (2)");
    assert_eq!(
        std::fs::metadata(&input).expect("stat").len(),
        before,
        "nothing was written"
    );
}

#[test]
fn an_unknown_site_is_an_error_and_names_what_the_library_has() {
    let input = fixture_copy("badsite.odb");
    let out = Command::new(BIN)
        .args([
            "run",
            input.to_str().unwrap(),
            "--die-area",
            "0 0 100 100",
            "--core-area",
            "10 10 90 90",
            "--site",
            "no_such_site_xyz",
        ])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2), "usage error, not a verdict");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(SITE),
        "the message must list the real sites, said: {err}"
    );
}

#[test]
fn the_binary_answers_help_and_describe_without_a_database() {
    let (code, out) = run(&["--describe"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"name\": \"ifp\""), "{out}");
    let (code, out) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("--die-area"), "{out}");
}
