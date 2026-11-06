use super::*;

const TICK: i64 = 25;

fn ms(t: i64) -> UtcMillis {
    UtcMillis(t)
}

fn p(ticks: i64) -> Price {
    Price(600_000 + ticks * TICK)
}

fn trail() -> Trail {
    Trail::new(TICK, DEFAULT_HALF_LIFE_MS)
}

#[test]
fn aggressors_set_the_balance_and_unknowns_follow_the_tick() {
    let mut t = trail();
    t.trade(ms(0), p(0), Qty(4), Some(Aggressor::Buy));
    t.trade(ms(0), p(1), Qty(3), Some(Aggressor::Sell));
    t.trade(ms(0), p(1), Qty(1), Some(Aggressor::Buy));
    // No aggressor: up from the previous price counts as bought, down as sold, unchanged as
    // the previous side.
    t.trade(ms(0), p(2), Qty(2), None);
    t.trade(ms(0), p(2), Qty(2), None);
    t.trade(ms(0), p(-1), Qty(5), None);
    let cell = |k| t.cell(t.row_of(p(k)));
    assert_eq!(cell(0).net, 1.0);
    assert_eq!(cell(1).net, -0.5, "3 sold, 1 bought");
    assert_eq!(cell(2).net, 1.0, "uptick, then unchanged: both bought");
    assert_eq!(cell(-1).net, -1.0, "downtick");
    assert!(cell(0).alpha > 0.0 && cell(5).alpha == 0.0);
    // The very first trade without an aggressor is neutral.
    let mut u = trail();
    u.trade(ms(0), p(0), Qty(2), None);
    assert_eq!(u.cell(u.row_of(p(0))).net, 0.0);
}

#[test]
fn rows_follow_the_dom_grid() {
    // Grouping 4: prices in one 4-tick row share a cell (floor, also below zero).
    let mut t = Trail::new(4 * TICK, DEFAULT_HALF_LIFE_MS);
    assert_eq!(t.row_of(Price(0)), 0);
    assert_eq!(t.row_of(Price(99)), 0);
    assert_eq!(t.row_of(Price(-1)), -1);
    t.trade(ms(0), Price(100), Qty(1), Some(Aggressor::Buy));
    t.trade(ms(0), Price(175), Qty(1), Some(Aggressor::Sell));
    assert_eq!(t.cell(1).net, 0.0, "one row, one each");
}

#[test]
fn decays_by_half_life_on_the_market_clock() {
    let mut t = trail();
    t.trade(ms(1_000), p(0), Qty(10), Some(Aggressor::Buy));
    t.trade(ms(1_000), p(3), Qty(10), Some(Aggressor::Buy));
    // A trade starts the trail at full strength; let the scale settle on the first step.
    let row = t.row_of(p(0));
    assert_eq!(t.cell(row).alpha, MAX_ALPHA);
    t.advance(ms(1_001));
    let a0 = t.cell(row).alpha;
    t.advance(ms(1_001 + i64::from(DEFAULT_HALF_LIFE_MS)));
    let a1 = t.cell(row).alpha;
    // Opacity is sqrt(volume / scale): half the volume is 1/sqrt(2) the opacity.
    assert!(
        (a1 / a0 - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-4,
        "{a0} {a1}"
    );
    // The clock never runs backwards.
    t.advance(ms(0));
    assert_eq!(t.cell(row).alpha, a1);
    // Same inputs, same cells (deterministic: no wall clock anywhere).
    let mut u = trail();
    u.trade(ms(1_000), p(0), Qty(10), Some(Aggressor::Buy));
    u.trade(ms(1_000), p(3), Qty(10), Some(Aggressor::Buy));
    u.advance(ms(1_001));
    u.advance(ms(1_001 + i64::from(DEFAULT_HALF_LIFE_MS)));
    assert_eq!(u.cell(row), t.cell(row));
}

#[test]
fn animation_stops_after_about_ten_half_lives() {
    for hl in [MIN_HALF_LIFE_MS, DEFAULT_HALF_LIFE_MS, MAX_HALF_LIFE_MS] {
        let mut t = Trail::new(TICK, hl);
        assert!(!t.needs_animation(ms(0)), "empty");
        for i in 0..20 {
            t.trade(ms(i * 50), p(i % 5), Qty(1 + i % 3), Some(Aggressor::Buy));
        }
        let end = 19 * 50;
        let at = |n: f64| ms(end + (f64::from(hl) * n) as i64);
        assert!(t.needs_animation(at(1.0)));
        assert!(t.needs_animation(at(5.0)));
        assert!(!t.needs_animation(at(10.5)), "half-life {hl}");
        // It agrees with paint: nothing is drawn any more.
        assert!((-10..10).all(|k| t.cell(t.row_of(p(k))).alpha == 0.0));
        assert!(!t.needs_animation(at(11.0)));
    }
}

#[test]
fn one_big_print_doesnt_blank_the_rest() {
    let mut t = trail();
    // A steady flow of small trades on a few rows sets the scale.
    for i in 0..200 {
        t.trade(ms(i * 20), p(i % 4), Qty(2), Some(Aggressor::Sell));
        t.advance(ms(i * 20));
    }
    let row = t.row_of(p(1));
    let before = t.cell(row).alpha;
    // A block 100× the usual size on another row.
    t.trade(ms(4_000), p(6), Qty(200), Some(Aggressor::Buy));
    t.advance(ms(4_016));
    let after = t.cell(row).alpha;
    assert_eq!(t.cell(t.row_of(p(6))).alpha, MAX_ALPHA, "capped");
    assert!(
        after > before * 0.8,
        "the scale isn't the instantaneous max: {before} -> {after}"
    );
}

#[test]
fn recentres_without_losing_rows_still_in_the_window() {
    let mut t = trail();
    t.trade(ms(0), p(0), Qty(5), Some(Aggressor::Buy));
    t.trade(ms(0), p(200), Qty(5), Some(Aggressor::Buy));
    // 400 rows up: outside the window around row 0 (±256), so it recentres there. Row 200
    // is still inside the new window; row 0 is 400 away (outside).
    t.trade(ms(0), p(400), Qty(5), Some(Aggressor::Buy));
    assert!(t.cell(t.row_of(p(200))).alpha > 0.0);
    assert_eq!(t.cell(t.row_of(p(0))).alpha, 0.0);
    // A slot reused by a far row starts empty, not with the old row's volume.
    let far = p(400 + ROWS as i64);
    t.trade(ms(0), far, Qty(1), Some(Aggressor::Sell));
    assert_eq!(t.cell(t.row_of(far)).net, -1.0);
    // A jump far away clears everything.
    t.trade(ms(0), p(100_000), Qty(1), Some(Aggressor::Buy));
    assert_eq!(t.cell(t.row_of(far)).alpha, 0.0);
}

#[test]
fn chevron_direction_and_settings() {
    let mut t = trail();
    assert_eq!(t.newest(), None);
    t.trade(ms(0), p(0), Qty(1), None);
    assert_eq!(t.newest(), Some((t.row_of(p(0)), None)));
    t.trade(ms(1), p(2), Qty(1), None);
    t.trade(ms(2), p(2), Qty(1), None);
    assert_eq!(
        t.newest(),
        Some((t.row_of(p(2)), Some(Move::Up))),
        "unchanged keeps it"
    );
    t.trade(ms(3), p(1), Qty(1), None);
    assert_eq!(t.newest().unwrap().1, Some(Move::Down));
    t.set_half_life(99);
    assert_eq!(t.half_life_ms(), MIN_HALF_LIFE_MS);
    t.set_half_life(60_000);
    assert_eq!(t.half_life_ms(), MAX_HALF_LIFE_MS);
    assert_eq!(next_half_life(1_500), 3_000);
    assert_eq!(next_half_life(5_000), 500);
    assert_eq!(next_half_life(1_200), 1_500, "custom rejoins the presets");
    t.clear(TICK * 2);
    assert_eq!((t.newest(), t.units()), (None, TICK * 2));
}
