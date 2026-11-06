//! Write the synthetic seed corpora: `cargo run -p lines-fuzz --example gen_corpus -- <dir>`
//! creates `<dir>/<target>/<seed>` for every fuzz target (decision 0010: synthetic only).

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "fuzz/corpus".into()),
    );
    let seeds = lines_fuzz::seeds::all();
    for target in lines_fuzz::seeds::TARGETS {
        std::fs::create_dir_all(dir.join(target))?;
    }
    for s in &seeds {
        std::fs::write(dir.join(s.target).join(&s.name), &s.data)?;
    }
    println!("{} seeds written to {}", seeds.len(), dir.display());
    Ok(())
}
