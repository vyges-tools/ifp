# vyges-ifp

Floorplan initialization over the OpenDB design database: set the die area, snap the core to the
site grid, build the rows, and store the core area those rows actually cover.

This is the first step of a physical flow — everything downstream (placement, power grid, tapcells,
routing) is positioned against the row grid this engine lays down. It is also the first Loom engine
that **writes** to the database rather than reading it.

```text
vyges physical ifp run design.odb --die-area '0 0 1000 1000' --core-area '10 10 990 990' --site FreePDK45_38x28_10R_NP_162NW_34O
```

## What it does

Given a die rectangle, a core rectangle and a site, it produces:

- the **die area**, snapped to the manufacturing grid if the technology states one;
- the **core area**, whose lower left is snapped *up* to the site grid;
- a **row grid** tiling the core, alternating R0/MX so abutting rows share power rails;
- the **core area as stored**, replaced by what the rows cover.

Rows are rebuilt, not appended: any existing rows are cleared first, so re-running after a die
change leaves no remnant of the previous grid.

## Two behaviours worth knowing before you read the output back

Both are upstream `initialize_floorplan` behaviours, reproduced deliberately, and both surprise
callers who expect their arguments to survive:

**The core's lower left moves; its upper right does not.** The lower left is rounded *up* to a whole
site so rows begin on a legal boundary. The upper right is left exactly where it was asked for,
because the row and site *counts* are what decide where the core really ends. When this moves, the
engine says so (`IFP-CORE-SNAP`, upstream IFP-28) rather than adjusting quietly.

**The core area you read back is not the core area you asked for.** After the rows are built, the
stored core area is replaced by the rectangle the rows cover. Ask for a core 1000 sites wide and
get 999 if the last one did not fit.

## Units

Die and core are given in **microns**, matching the upstream Tcl argument, and converted using the
database's own `dbu_per_micron`. A database with no DBU scale is an error, not an assumed 1000.

## What it does not do

- The **utilization / aspect-ratio form** of `initialize_floorplan`, which derives a die from the
  placed cell area, is not implemented. Give explicit rectangles.
- It does not place or legalize anything. Cells already sitting on an old row grid are not moved.
- It does not add tapcells, well taps, endcaps or a power grid; those are separate engines.

## Correctness — measured, not claimed

The algorithm is written from the published behaviour of `initialize_floorplan` and checked against
the upstream `ifp` regression goldens at pin `b5624809f29048e1f9ce9e83eb562620c652e084`. It is a
reimplementation, not a transliteration: where the two disagree, the goldens are the arbiter.

The rules are stated separately from the code, and the arithmetic is split from
the database so every rule is testable without an `.odb`. The reference case in the unit tests
reproduces each number in `init_floorplan1.ok` — the snapped origin, the row count, the sites per
row, and the final core rectangle — and the integration tests confirm those numbers survive the
write and a read-back through an independent reader.

Against the upstream suite, at a pinned OpenROAD commit:

| | |
| --- | --- |
| **23 pass** | every compared `IFP-*` line identical to the golden |
| **2 fail** | the one gap below |
| **15 not comparable** | 6 utilization form, 6 polygon floorplans, 3 cases that never call `initialize_floorplan` |

The comparison is on the `IFP-*` log lines, because those lines *are* what the algorithm decided:
the snapped core origin, the row and site counts, and the resulting bboxes. Lines this engine does
not emit are reported by the harness rather than dropped, so the pass count cannot flatter itself.

### The one known gap

**UPF power domains** (2 cases). Upstream's floorplan inserts power-domain instances, so its
instance count rises (16 → 40 on `upf_test`). This engine inserts none. All floorplan *geometry*
matches exactly on both cases; what differs is the census that follows from the count — IFP-0103,
IFP-0104 and IFP-0105 together.

## Hybrid sites

A **hybrid** site tiles the core from a repeating *row pattern* of other sites rather than from one
height. Two sets of rows come out of it, and both are real:

- the pattern's **member** sites, laid down in sequence, taking their orientation from the pattern
  rather than from the R0/MX alternation (IFP-0049);
- each **hybrid** site itself, one row per whole pattern (IFP-0050).

A second hybrid site is offset to wherever its pattern occurs inside the base site's, so its rows
land on the same boundaries. It matches either as written (`R0`) or reversed with every orientation
mirrored (`MX`) — the same sequence read from the other end. Matching neither is an error.

Row parity is **refused** on a hybrid floorplan rather than applied: parity would have to trim whole
patterns, not rows, and silently trimming the wrong unit is worse than declining.

A partial pattern at the top of the core is not built — the sequence simply stops where the next
entry would overflow.

## Site order

Sites are visited in **name order**, deduplicated by name — not in the order given on the command
line. Both the row numbering and the order of the log lines follow from that. The set is also wider
than the arguments: it includes sites used by placed instances that were never named (macros
excluded), because a hybrid library can place cells on a site nobody mentioned.

## The fit check

A master larger than the core is refused (IFP-0002) before anything is snapped or written. Three
details are upstream's, and each one changes the answer:

- the check runs **after** the die tests, so a design with both an empty die and an oversized macro
  reports the die — the error a caller can actually act on;
- **pads and covers are exempt**, since they belong outside the core and their size says nothing
  about whether the core is big enough;
- a master with **R90 symmetry** is free to rotate, so it only has to fit the core's *larger*
  dimension rather than width-against-width.

The area census (IFP-0103, IFP-0104) deliberately disagrees with the fit check about pads: it counts
**every** instance, because it is a census of the design rather than a question about the core.

## Report

```json
{
  "tool": "vyges-ifp",
  "status": "applied",
  "dbu_per_micron": 1000,
  "die_area":            { "dbu": [0, 0, 100000, 100000], "um": [0.0, 0.0, 100.0, 100.0] },
  "core_area_requested": { "dbu": [10000, 10000, 90000, 90000], "um": [10.0, 10.0, 90.0, 90.0] },
  "core_area_snapped":   { "dbu": [10120, 10880, 90000, 90000], "um": [10.12, 10.88, 90.0, 90.0] },
  "core_area":           { "dbu": [10120, 10880, 89700, 89760], "um": [10.12, 10.88, 89.7, 89.76] },
  "core_was_snapped": true,
  "rows": 29,
  "sites_per_row": 173,
  "rows_per_site": [ { "site": "unithd", "rows": 29 } ],
  "odb_written": "design.odb"
}
```

`core_area_requested`, `core_area_snapped` and `core_area` are reported separately precisely because
they differ; a caller that needs to know whether its floorplan was honoured can compare them without
re-deriving the arithmetic.

## Writing

By default the database is written **in place**, over the input — that is what a flow step does.
Pass `--out-odb FILE` to write elsewhere, or `--dry-run` to plan and report without writing.

A refused plan writes nothing at all: the plan is built entirely before the database is touched.

## Exit status

| | |
| --- | --- |
| `0` | applied — the floorplan was built and written |
| `1` | refused — the design cannot be floorplanned as asked (empty die, core outside the die, degenerate or mismatched site, or no row fits) |
| `2` | error — usage, unreadable database, no DBU scale, or a failed write |

A refusal is a verdict about the design and is distinct from an error about the run, so a flow can
tell "this floorplan is impossible" from "the tool broke".

## Building

```text
cargo build --release
cargo test
```

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
