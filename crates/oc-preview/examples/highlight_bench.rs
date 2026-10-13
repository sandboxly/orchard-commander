//! Times syntax highlighting of 256 KB of source (the highlight limit).
//!
//! `cargo run -p oc-preview --release --example highlight_bench`

use std::time::Instant;

use oc_preview::highlight::{MAX_HIGHLIGHT_BYTES, highlight_lines, syntax_for_file, syntax_set};

const PY: &str = r#"
@dataclass(frozen=True)
class Crossing:
    """A ferry crossing with "quotes" and 'strings'."""
    route: str
    seats_left: int = 0  # comment with TODO

    @property
    def sold_out(self) -> bool:
        return self.seats_left <= 0 and not {"a": [1, 2.5, 0x1f]}

def crossings_for(route: str, day: date) -> list[Crossing]:
    return [c for c in FIXTURE if c.route == route and c.day == day]
"#;

const RS: &str = r#"
/// Doc comment with `code`.
pub fn resolve_target(sources: &[VPath], target: &VPath) -> Result<Vec<(VPath, VPath)>, Error> {
    if target.is_dir() {
        return Ok(sources.iter().map(|s| (s.clone(), target.child(&[&s.name()]))).collect());
    }
    let x: u64 = 0x1f + 42 * "str".len() as u64; // trailing comment
    match x { 0 => {}, _ => println!("{x:?}") }
    Err(Error::Operation(format!("Target directory does not exist: {}", target.display())))
}
"#;

fn main() {
    let t = Instant::now();
    let _ = syntax_set();
    println!("load syntaxes: {:?}", t.elapsed());
    for (name, sample) in [("x.py", PY), ("x.rs", RS)] {
        let text = sample.repeat(MAX_HIGHLIGHT_BYTES / sample.len());
        let syntax = syntax_for_file(name).expect("known syntax");
        // First run compiles the grammar's regexes.
        let t = Instant::now();
        let lines = highlight_lines(&text, syntax, &|| false).expect("highlighted");
        let first = t.elapsed();
        let t = Instant::now();
        highlight_lines(&text, syntax, &|| false).expect("highlighted");
        println!(
            "{name}: {} KB, {} lines: first {first:?}, again {:?}",
            text.len() / 1024,
            lines.len(),
            t.elapsed()
        );
    }
}
