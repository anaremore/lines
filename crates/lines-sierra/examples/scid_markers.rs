//! Dev diagnostic: characterize Sierra's `open`-field markers in one UTC day of a `.scid`.
//! `cargo run -p lines-sierra --example scid_markers -- <file.scid> <YYYY-MM-DD>`

use std::collections::BTreeMap;
use std::path::Path;

use lines_core::UtcMillis;
use lines_sierra::format::{ScidReader, sc_to_unix_us, unix_us_to_sc};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let day = UtcMillis::parse_iso(&format!("{}T00:00:00", args[2]))
        .expect("date")
        .0
        * 1000;
    let mut r = ScidReader::open(Path::new(&args[1]))?;
    r.seek_to_time(unix_us_to_sc(day))?;
    let mut recs = Vec::new();
    while let Some(rec) = r.next_record()? {
        if sc_to_unix_us(rec.time) >= day + 86_400_000_000 {
            break;
        }
        recs.push(rec);
    }
    let mut opens: BTreeMap<u32, (u64, String)> = BTreeMap::new();
    for rec in &recs {
        let e = opens
            .entry(rec.open.to_bits())
            .or_insert((0, format!("{:.9e}", rec.open)));
        e.0 += 1;
    }
    println!("distinct open values (bits, count, value):");
    for (bits, (n, v)) in &opens {
        println!("  {bits:#010x} {n:>7} {v}");
    }
    // Pair structure, both ways round: for each A find the next B. The true opener gives
    // brackets inside one millisecond (Sierra unbundles one aggressor order); the wrong one
    // pairs the end of one bracket with the start of the next. (On NQ the opener is
    // 0xfd709ef9: see `format::FIRST_SUB_TRADE`.)
    let markers: Vec<u32> = opens.keys().copied().filter(|b| *b != 0).collect();
    for (a, b) in markers
        .first()
        .zip(markers.get(1))
        .map(|(&x, &y)| [(x, y), (y, x)])
        .into_iter()
        .flatten()
    {
        let (mut gaps, mut same_price, mut diff_price, mut same_time, mut unmatched) =
            (BTreeMap::<usize, u64>::new(), 0, 0, 0, 0);
        let mut vol_inside = 0u64;
        let mut i = 0;
        while i < recs.len() {
            if recs[i].open.to_bits() == a {
                match (i + 1..recs.len()).find(|&j| recs[j].open.to_bits() == b) {
                    Some(j) => {
                        *gaps.entry(j - i - 1).or_default() += 1;
                        if recs[i].close == recs[j].close {
                            same_price += 1
                        } else {
                            diff_price += 1
                        }
                        if recs[i].time / 1000 == recs[j].time / 1000 {
                            same_time += 1
                        }
                        vol_inside += (i..=j).map(|k| recs[k].total_volume as u64).sum::<u64>();
                        i = j;
                    }
                    None => unmatched += 1,
                }
            }
            i += 1;
        }
        println!("A={a:#010x} -> B={b:#010x}: records between A and B {gaps:?}");
        println!(
            "  same price {same_price}, different price {diff_price}, same ms {same_time}, unmatched A {unmatched}, volume A..=B {vol_inside}"
        );
        let total: u64 = recs.iter().map(|r| r.total_volume as u64).sum();
        println!("  day volume (all records) {total}");
    }
    Ok(())
}
