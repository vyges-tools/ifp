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

// ---- power intent: made physical inside row building, by the power-intent engine ----

/// The fixture with one power domain declared, so the floorplan has power intent to evaluate.
fn fixture_with_power_domain(name: &str) -> PathBuf {
    let p = fixture_copy(name);
    let mut db = Db::open(&p).expect("opens");
    assert!(db.pwr_domain_create("PD_TOP").expect("creates"), "a fresh domain");
    db.write(&p).expect("writes");
    p
}

/// A stand-in evaluator: records its arguments, says `lines`, writes `result` (or its own
/// input, when `None`) as its `-o` output, and exits `code`. Its argv is
/// `eval IN -o OUT [--liberty F]...`.
fn fake_evaluator(name: &str, lines: &str, code: i32, result: Option<&std::path::Path>) -> PathBuf {
    let p = scratch(name);
    let args = scratch(&format!("{name}.args"));
    let source = result.map(|r| format!("'{}'", r.display())).unwrap_or_else(|| "\"$2\"".into());
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\necho \"$@\" > '{}'\nprintf '%s' '{lines}'\ncp {source} \"$4\"\nexit {code}\n",
            args.display()
        ),
    )
    .expect("writes the stand-in");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    p
}

fn run_with_evaluator(evaluator: &std::path::Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .args(args)
        .env("VYGES_UPF_BIN", evaluator)
        .output()
        .expect("the binary runs");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).to_string())
}

fn floorplan_args<'a>(input: &'a std::path::Path, core: &'a str) -> Vec<&'a str> {
    vec![
        "run", input.to_str().unwrap(), "--die-area", "0 0 100 100", "--core-area", core,
        "--site", SITE, "--liberty", "a.lib", "--liberty", "b.lib",
    ]
}

/// Rule: the evaluation happens INSIDE row building, before the core-snap warning and the row
/// count are reported, and the evaluator is handed the Liberty files in their read order.
#[test]
fn power_intent_is_evaluated_before_the_rows_are_reported() {
    let input = fixture_with_power_domain("pi-order.odb");
    let ev = fake_evaluator("ev-order", "[WARNING UPF-0024] stand-in line\n", 0, None);
    let (code, err) = run_with_evaluator(&ev, &floorplan_args(&input, "10 10 90 90"));
    assert_eq!(code, 0, "{err}");
    let at = |s: &str| err.find(s).unwrap_or_else(|| panic!("no `{s}` in {err}"));
    assert!(at("[WARNING UPF-0024] stand-in line") < at("IFP-0028"), "{err}");
    assert!(at("IFP-0028") < at("IFP-0001"), "{err}");
    let args = std::fs::read_to_string(scratch("ev-order.args")).expect("the evaluator ran");
    assert!(args.starts_with("eval "), "{args}");
    assert!(args.trim_end().ends_with("--liberty a.lib --liberty b.lib"), "{args}");
}

/// Rule: the floorplan goes on from the EVALUATED database, so the closing instance census
/// counts the cells the evaluation inserted.
#[test]
fn the_census_counts_what_the_evaluation_inserted() {
    let input = fixture_with_power_domain("pi-census.odb");
    let before = Db::open(&input).expect("opens").num_insts();
    let evaluated = fixture_with_power_domain("pi-census-evaluated.odb");
    {
        let mut db = Db::open(&evaluated).expect("opens");
        let master = db.inst_master(&db.inst_names()[0]);
        assert!(db.inst_try_create(&master, "inserted_by_evaluation").expect("creates"));
        db.write(&evaluated).expect("writes");
    }
    let ev = fake_evaluator("ev-census", "", 0, Some(&evaluated));
    let (code, err) = run_with_evaluator(&ev, &floorplan_args(&input, "10 10 90 90"));
    assert_eq!(code, 0, "{err}");
    let census = err.lines().find(|l| l.contains("IFP-0105")).unwrap_or_else(|| panic!("{err}"));
    let count: usize = census
        .split(|c: char| !c.is_ascii_digit())
        .rfind(|t| !t.is_empty())
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("{census}"));
    assert_eq!(count, before + 1, "{census}");
}

/// Rule: the evaluator's stops are the floorplan's: a reported error stops it (1), a step it
/// does not implement is refused (3), and a failure without a message is an error (2).
#[test]
fn the_evaluators_stops_become_the_floorplans() {
    for (name, lines, code, want) in [
        ("ev-error", "[ERROR UPF-0029] No TOP DOMAIN found, aborting\n", 2, 1),
        ("ev-refused", "", 3, 3),
        ("ev-broken", "", 2, 2),
    ] {
        let input = fixture_with_power_domain(&format!("{name}.odb"));
        let before = std::fs::read(&input).expect("reads");
        let ev = fake_evaluator(name, lines, code, None);
        let (got, err) = run_with_evaluator(&ev, &floorplan_args(&input, "10 10 90 90"));
        assert_eq!(got, want, "{name}: {err}");
        assert!(!err.contains("IFP-0001"), "{name}: no rows reported after a stop: {err}");
        assert_eq!(std::fs::read(&input).expect("reads"), before, "{name}: nothing written");
    }
}

/// Rule: a design with power intent is never floorplanned without its evaluation.
#[test]
fn power_intent_without_an_evaluator_is_an_error_not_a_quiet_floorplan() {
    let input = fixture_with_power_domain("pi-missing.odb");
    let (code, err) = run_with_evaluator(
        std::path::Path::new("/nonexistent/vyges-upf"),
        &floorplan_args(&input, "10 10 90 90"),
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("power intent"), "{err}");
}

/// Rule: nothing is evaluated when the core's lower left is negative (the row builder's guard),
/// nor when the design declares no power intent at all.
#[test]
fn nothing_is_evaluated_without_intent_or_with_a_negative_core() {
    let missing = std::path::Path::new("/nonexistent/vyges-upf");
    let plain = fixture_copy("pi-none.odb");
    let (code, err) = run_with_evaluator(missing, &floorplan_args(&plain, "10 10 90 90"));
    assert_eq!(code, 0, "no intent, no evaluator needed: {err}");

    // A negative core: the evaluator must not have been asked (asking a missing one is exit 2).
    let input = fixture_with_power_domain("pi-negative.odb");
    let (code, err) = run_with_evaluator(missing, &[
        "run", input.to_str().unwrap(), "--die-area", "-50 -50 100 100", "--core-area",
        "-10 -10 90 90", "--site", SITE,
    ]);
    assert_ne!(code, 2, "{err}");
    assert!(!err.contains("power intent"), "{err}");
}
