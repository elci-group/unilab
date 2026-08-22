# Uni

Uni runs a deterministic, concurrent analysis suite against one project and
normalizes each application's output into a single graded report.

The default suite is Amber, Ami, Bart, Chakra, Ferret hunt, Fract, Isopod,
Lwoodz, Tempcheq, Traci, and Vamos. Jeenome is opt-in because it requires an
`strace` trace. Ferret hunt uses Ferret's `track` operation with an in-memory
corpus, so the analysis does not write `ferret.db` into the target.

## Usage

```bash
cargo run -- /path/to/project
cargo run -- --json --only ferret,tempcheq /path/to/project
cargo run -- analyze --fail-under 80 /path/to/project
```

Use `--skip` to exclude applications and `--tools-dir` to choose where sibling
application repositories live.

## Missing applications

When a selected application is not installed, Uni clones its associated
`https://github.com/elci-group/<application>.git` repository into the tools
directory and runs `baby --user` from that checkout. If Git, Baby, cloning, or
installation fails, Uni records the application as unavailable and continues
building the rest of the report.

## Ferret scoring

Ferret findings reduce its 100-point score by severity: 25 points for a
critical finding, 15 for major, 5 for minor, and 1 for informational. Critical
or major findings fail the Ferret check; lower-severity findings warn.

## Development

```bash
cargo fmt -- --check
cargo test
```
