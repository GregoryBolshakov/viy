use pretty_assertions::assert_eq;

use super::latex;

/// rpctl tests/test_markdown.py `test_latex`.
#[test]
fn rp_cases() {
    assert_eq!(latex(r"\alpha + \beta_{1}"), "α + β₁");
    assert_eq!(latex(r"\frac{n(n+1)}{2}"), "(n(n+1))/2");
    assert_eq!(latex(r"90^\circ"), "90°");
    assert_eq!(latex(r"\boxed{42}"), "[ 42 ]");
}

#[test]
fn the_sample_formula() {
    assert_eq!(latex(r"\frac{a}{b} \le \sqrt{c}"), "a/b ≤ √c");
    assert_eq!(latex("x^2"), "x²");
}

#[test]
fn scripts_without_a_unicode_form_keep_their_mark() {
    assert_eq!(latex("x^{a+b}"), "x^(a+b)");
    assert_eq!(latex("x^{a+e}"), "xᵃ⁺ᵉ");
    assert_eq!(latex("x^{q}"), "x^q");
    assert_eq!(latex("x_{qr}"), "x_(qr)");
    assert_eq!(latex("f'^\\prime"), "f'′");
}

#[test]
fn roots_fonts_and_accents() {
    assert_eq!(latex(r"\sqrt[3]{x}"), "∛x");
    assert_eq!(latex(r"\sqrt{x+1}"), "√(x+1)");
    assert_eq!(latex(r"\mathbb{R}^n"), "ℝⁿ");
    assert_eq!(latex(r"\text{if } x"), "if  x");
    assert_eq!(latex(r"\hat{x}"), "x\u{302}");
    assert_eq!(latex(r"\left( x \right."), "( x ");
}

#[test]
fn unknown_commands_lose_only_the_backslash() {
    assert_eq!(latex(r"\foo{x}"), "foox");
    assert_eq!(latex(r"a \\ b"), "a ;  b");
    assert_eq!(latex(r"\{x\}"), "{x}");
}

/// rp's regex finds no command after a final backslash and raises; viy
/// shows it.
#[test]
fn a_final_backslash_is_shown() {
    assert_eq!(latex("x \\"), "x \\");
    assert_eq!(latex("\\"), "\\");
}

#[test]
fn unclosed_groups_take_the_rest() {
    assert_eq!(latex(r"\frac{a"), "a/");
    assert_eq!(latex("x^"), "x^()");
}
