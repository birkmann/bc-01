//! Seeded hash ordering (PLAN 3.4): a stable, pageable shuffle that replaces
//! `ORDER BY random()`. SQL form: `(id * 2654435761 + seed) % 4294967296`.

pub const MULT: i64 = 2_654_435_761;
pub const MOD: i64 = 4_294_967_296;

/// `(id*2654435761 + seed) % 2^32`, identical to the SQL expression (i64 arithmetic cannot
/// overflow for ids < 3.4e9).
pub fn hash(id: i64, seed: i64) -> i64 {
    (id.wrapping_mul(MULT).wrapping_add(seed)).rem_euclid(MOD)
}

/// SQL ORDER BY fragment for a column, with the seed inlined (it is an integer).
pub fn sql_order(id_column: &str, seed: i64) -> String {
    format!("(({id_column} * {MULT} + {seed}) % {MOD})")
}

/// Stable per-(track, seed) jitter in `[0, 1)`; used by the `similar` reroll.
pub fn unit(id: i64, seed: i64) -> f64 {
    hash(id, seed) as f64 / MOD as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_sql_expression() {
        assert_eq!(hash(1, 0), 2_654_435_761);
        assert_eq!(hash(2, 0), (2 * 2_654_435_761_i64) % MOD);
        assert_eq!(hash(7, 3), (7 * 2_654_435_761_i64 + 3) % MOD);
    }

    #[test]
    fn unit_is_in_range_and_stable() {
        for id in 1..1000 {
            let u = unit(id, 5);
            assert!((0.0..1.0).contains(&u));
            assert_eq!(u, unit(id, 5));
        }
    }
}
