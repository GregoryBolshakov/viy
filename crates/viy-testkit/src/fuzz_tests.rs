use pretty_assertions::assert_eq;

use super::*;

#[test]
fn same_seed_same_sequence() {
    let mut a = Gen::new(7);
    let mut b = Gen::new(7);
    let xs: Vec<u64> = (0..5).map(|_| a.next_u64()).collect();
    let ys: Vec<u64> = (0..5).map(|_| b.next_u64()).collect();
    assert_eq!(xs, ys);
    assert_ne!(Gen::new(8).next_u64(), xs[0]);
}

#[test]
fn strings_use_only_the_pieces() {
    let mut g = Gen::new(1);
    for _ in 0..200 {
        let s = g.string(&["ab", "ц"], 4);
        let rest = s.replace("ab", "").replace('ц', "");
        assert_eq!(rest, "", "{s:?}");
        assert!(s.chars().count() <= 8);
    }
}

#[test]
fn below_stays_below() {
    let mut g = Gen::new(2);
    assert!((0..1000).all(|_| g.below(3) < 3));
}
