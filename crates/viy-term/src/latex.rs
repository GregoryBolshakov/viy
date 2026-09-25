//! LaTeX math as plain Unicode for the terminal: rp's `markdown.latex`
//! (flags F34). Best effort: Greek letters and symbols by name, `\frac`,
//! roots, sub- and superscripts where every char has a Unicode form, and
//! unknown commands lose only their backslash, so nothing is dropped.

/// `\alpha`, `\Gamma`, ...
const GREEK: &[(&str, &str)] = &[
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ε"),
    ("varepsilon", "ε"),
    ("zeta", "ζ"),
    ("eta", "η"),
    ("theta", "θ"),
    ("vartheta", "ϑ"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("nu", "ν"),
    ("xi", "ξ"),
    ("pi", "π"),
    ("varpi", "ϖ"),
    ("rho", "ρ"),
    ("varrho", "ϱ"),
    ("sigma", "σ"),
    ("varsigma", "ς"),
    ("tau", "τ"),
    ("upsilon", "υ"),
    ("phi", "φ"),
    ("varphi", "φ"),
    ("chi", "χ"),
    ("psi", "ψ"),
    ("omega", "ω"),
    ("Gamma", "Γ"),
    ("Delta", "Δ"),
    ("Theta", "Θ"),
    ("Lambda", "Λ"),
    ("Xi", "Ξ"),
    ("Pi", "Π"),
    ("Sigma", "Σ"),
    ("Upsilon", "Υ"),
    ("Phi", "Φ"),
    ("Psi", "Ψ"),
    ("Omega", "Ω"),
];

const SYMBOLS: &[(&str, &str)] = &[
    ("times", "×"),
    ("cdot", "·"),
    ("div", "÷"),
    ("pm", "±"),
    ("mp", "∓"),
    ("ast", "∗"),
    ("le", "≤"),
    ("leq", "≤"),
    ("ge", "≥"),
    ("geq", "≥"),
    ("ne", "≠"),
    ("neq", "≠"),
    ("approx", "≈"),
    ("equiv", "≡"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("cong", "≅"),
    ("propto", "∝"),
    ("ll", "≪"),
    ("gg", "≫"),
    ("infty", "∞"),
    ("partial", "∂"),
    ("nabla", "∇"),
    ("sum", "∑"),
    ("prod", "∏"),
    ("int", "∫"),
    ("iint", "∬"),
    ("oint", "∮"),
    ("in", "∈"),
    ("notin", "∉"),
    ("ni", "∋"),
    ("subset", "⊂"),
    ("subseteq", "⊆"),
    ("supset", "⊃"),
    ("supseteq", "⊇"),
    ("cup", "∪"),
    ("cap", "∩"),
    ("emptyset", "∅"),
    ("varnothing", "∅"),
    ("setminus", "∖"),
    ("forall", "∀"),
    ("exists", "∃"),
    ("nexists", "∄"),
    ("neg", "¬"),
    ("lnot", "¬"),
    ("land", "∧"),
    ("wedge", "∧"),
    ("lor", "∨"),
    ("vee", "∨"),
    ("oplus", "⊕"),
    ("otimes", "⊗"),
    ("to", "→"),
    ("rightarrow", "→"),
    ("leftarrow", "←"),
    ("gets", "←"),
    ("leftrightarrow", "↔"),
    ("Rightarrow", "⇒"),
    ("implies", "⇒"),
    ("Leftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("iff", "⇔"),
    ("mapsto", "↦"),
    ("uparrow", "↑"),
    ("downarrow", "↓"),
    ("longrightarrow", "⟶"),
    ("Longrightarrow", "⟹"),
    ("ldots", "…"),
    ("dots", "…"),
    ("cdots", "⋯"),
    ("vdots", "⋮"),
    ("ddots", "⋱"),
    ("circ", "∘"),
    ("degree", "°"),
    ("angle", "∠"),
    ("triangle", "△"),
    ("perp", "⊥"),
    ("parallel", "∥"),
    ("mid", "|"),
    ("vert", "|"),
    ("Vert", "‖"),
    ("lfloor", "⌊"),
    ("rfloor", "⌋"),
    ("lceil", "⌈"),
    ("rceil", "⌉"),
    ("langle", "⟨"),
    ("rangle", "⟩"),
    ("hbar", "ℏ"),
    ("ell", "ℓ"),
    ("Re", "ℜ"),
    ("Im", "ℑ"),
    ("aleph", "ℵ"),
    ("prime", "′"),
    ("star", "⋆"),
    ("bullet", "•"),
    ("therefore", "∴"),
    ("because", "∵"),
    ("top", "⊤"),
    ("bot", "⊥"),
    ("vdash", "⊢"),
    ("models", "⊨"),
    ("checkmark", "✓"),
    ("square", "□"),
    ("quad", "  "),
    ("qquad", "    "),
    ("lim", "lim"),
    ("log", "log"),
    ("ln", "ln"),
    ("exp", "exp"),
    ("sin", "sin"),
    ("cos", "cos"),
    ("tan", "tan"),
    ("max", "max"),
    ("min", "min"),
    ("det", "det"),
    ("gcd", "gcd"),
    ("mod", "mod"),
    ("bmod", "mod"),
    ("arg", "arg"),
    ("deg", "deg"),
];

/// `\mathbb{R}` and the other number sets.
const BLACKBOARD: &[(&str, &str)] = &[("R", "ℝ"), ("N", "ℕ"), ("Z", "ℤ"), ("Q", "ℚ"), ("C", "ℂ")];

const SUPERSCRIPT: (&str, &str) = ("0123456789+-=()niaeoxyk", "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿⁱᵃᵉᵒˣʸᵏ");
const SUBSCRIPT: (&str, &str) = ("0123456789+-=()aeoxijknmt", "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₐₑₒₓᵢⱼₖₙₘₜ");

/// Commands whose argument is shown as it is (or converted), without the
/// command: fonts, `\text`, accents.
const ARGUMENT_ONLY: &[&str] = &[
    "text",
    "textrm",
    "mathrm",
    "mathit",
    "operatorname",
    "textit",
    "mathsf",
    "mathtt",
    "textbf",
    "mathbf",
    "boldsymbol",
    "mathcal",
    "bm",
    "hat",
    "bar",
    "vec",
    "overline",
    "tilde",
    "dot",
    "mathbb",
];

/// Combining accents for one-letter arguments.
const ACCENTS: &[(&str, char)] = &[
    ("hat", '\u{302}'),
    ("bar", '\u{304}'),
    ("vec", '\u{20D7}'),
    ("overline", '\u{305}'),
    ("tilde", '\u{303}'),
    ("dot", '\u{307}'),
];

/// Sizing and spacing commands that draw nothing.
const INVISIBLE: &[&str] = &[
    "left",
    "right",
    "big",
    "Big",
    "bigg",
    "Bigg",
    "displaystyle",
    "limits",
    "nolimits",
];

fn lookup<V: Copy>(table: &[(&str, V)], key: &str) -> Option<V> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// LaTeX math `s` as plain Unicode.
pub fn latex(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    convert(&chars)
}

fn convert(s: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        match c {
            '\\' => {
                let Some((name, end)) = command(s, i) else {
                    // A lone backslash at the end (rp's regex finds no
                    // command there and raises).
                    out.push('\\');
                    i += 1;
                    continue;
                };
                i = end;
                i = command_text(&name, s, i, &mut out);
            }
            '^' | '_' => {
                let (arg, end) = group(s, i + 1);
                i = end;
                let arg: String = arg.iter().collect();
                if c == '^' && (arg == "\\circ" || arg == "{\\circ}") {
                    out.push('°');
                } else if c == '^' && (arg == "\\prime" || arg == "'") {
                    out.push('′');
                } else {
                    let table = if c == '^' { SUPERSCRIPT } else { SUBSCRIPT };
                    out.push_str(&script(&latex(&arg), table, c));
                }
            }
            '{' | '}' | '&' => i += 1,
            '~' => {
                out.push(' ');
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// What the command `name` (its arguments start at `i`) shows; returns
/// where the input continues.
fn command_text(name: &str, s: &[char], mut i: usize, out: &mut String) -> usize {
    match name {
        "frac" | "dfrac" | "tfrac" => {
            let (a, end) = group(s, i);
            let (b, end) = group(s, end);
            out.push_str(&format!("{}/{}", wrap(&convert(a)), wrap(&convert(b))));
            end
        }
        "sqrt" => {
            let mut n = String::new();
            if s.get(i) == Some(&'[')
                && let Some(j) = s[i..].iter().position(|&c| c == ']')
            {
                n = s[i + 1..i + j].iter().collect();
                i += j + 1;
            }
            let (a, end) = group(s, i);
            let root = match n.as_str() {
                "3" => "∛",
                "4" => "∜",
                _ => "√",
            };
            let inner = convert(a);
            out.push_str(root);
            if inner.chars().count() <= 1 {
                out.push_str(&inner);
            } else {
                out.push_str(&format!("({inner})"));
            }
            end
        }
        _ if ARGUMENT_ONLY.contains(&name) => {
            let (a, end) = group(s, i);
            let arg: String = a.iter().collect();
            if name == "mathbb"
                && let Some(symbol) = lookup(BLACKBOARD, &arg)
            {
                out.push_str(symbol);
                return end;
            }
            let mut inner = if name.starts_with("text") || name == "operatorname" {
                arg
            } else {
                convert(a)
            };
            if let Some(accent) = lookup(ACCENTS, name)
                && inner.chars().count() == 1
            {
                inner.push(accent);
            }
            out.push_str(&inner);
            end
        }
        "boxed" => {
            let (a, end) = group(s, i);
            out.push_str(&format!("[ {} ]", convert(a)));
            end
        }
        _ if INVISIBLE.contains(&name) => {
            if s.get(i) == Some(&'.') {
                i += 1;
            }
            i
        }
        "begin" | "end" => group(s, i).1,
        _ => {
            if let Some(letter) = lookup(GREEK, name).or_else(|| lookup(SYMBOLS, name)) {
                out.push_str(letter);
            } else {
                match name {
                    "," | ";" | ":" | " " => out.push(' '),
                    "!" => {}
                    "\\" => out.push_str("; "),
                    // `\{`, `\%`, ... and unknown commands: the name.
                    _ => out.push_str(name),
                }
            }
            i
        }
    }
}

/// The command at `s[i]` (a backslash): its name (a run of ASCII letters,
/// or one other char) and where it ends.
fn command(s: &[char], i: usize) -> Option<(String, usize)> {
    let first = *s.get(i + 1)?;
    if first.is_ascii_alphabetic() {
        let len = s[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        Some((s[i + 1..i + 1 + len].iter().collect(), i + 1 + len))
    } else {
        Some((first.to_string(), i + 2))
    }
}

/// The braced group or single token at `s[i..]`, spaces skipped: its
/// content and where it ends.
fn group(s: &[char], mut i: usize) -> (&[char], usize) {
    while s.get(i) == Some(&' ') {
        i += 1;
    }
    match s.get(i) {
        None => (&[], i),
        Some('{') => {
            let mut depth = 0;
            for (j, &c) in s.iter().enumerate().skip(i) {
                if c == '{' {
                    depth += 1;
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        return (&s[i + 1..j], j + 1);
                    }
                }
            }
            (&s[i + 1..], s.len())
        }
        Some('\\') => match command(s, i) {
            Some((_, end)) => (&s[i..end], end),
            None => (&s[i..=i], i + 1),
        },
        Some(_) => (&s[i..=i], i + 1),
    }
}

/// `x^2` → `x²`, `x_{ij}` → `xᵢⱼ` when every char has a Unicode form;
/// otherwise the mark stays: `x^a+b` → `x^(a+b)`.
fn script(text: &str, (from, to): (&str, &str), mark: char) -> String {
    let map = |c: char| {
        from.chars()
            .position(|f| f == c)
            .and_then(|k| to.chars().nth(k))
    };
    if !text.is_empty() && text.chars().all(|c| map(c).is_some()) {
        return text.chars().filter_map(map).collect();
    }
    if text.chars().count() == 1 {
        format!("{mark}{text}")
    } else {
        format!("{mark}({text})")
    }
}

/// A fraction's part, in parentheses unless it is one token.
fn wrap(t: &str) -> String {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
    let token = !t.is_empty() && t.chars().all(word);
    let parenthesized = t.starts_with('(') && t.ends_with(')') && t.len() >= 2 && !t.contains('\n');
    if t.chars().count() <= 1 || token || parenthesized {
        t.to_string()
    } else {
        format!("({t})")
    }
}

#[cfg(test)]
#[path = "latex_tests.rs"]
mod tests;
