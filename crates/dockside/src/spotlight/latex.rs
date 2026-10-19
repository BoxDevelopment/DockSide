//! LaTeX (KaTeX, as Notion uses) symbol and structure lookup for Spotlight:
//! type "\alpha", "\int", "latex arrow" or "tex sum" to find commands; Enter
//! copies the LaTeX code, Shift+Enter the Unicode symbol.

/// (LaTeX code, preview / Unicode symbol, search words)
pub const SYMBOLS: &[(&str, &str, &str)] = &[
    // Greek, lower case.
    (r"\alpha", "α", "alpha greek"),
    (r"\beta", "β", "beta greek"),
    (r"\gamma", "γ", "gamma greek"),
    (r"\delta", "δ", "delta greek"),
    (r"\epsilon", "ϵ", "epsilon greek"),
    (r"\varepsilon", "ε", "varepsilon epsilon greek"),
    (r"\zeta", "ζ", "zeta greek"),
    (r"\eta", "η", "eta greek"),
    (r"\theta", "θ", "theta greek angle"),
    (r"\vartheta", "ϑ", "vartheta theta greek"),
    (r"\iota", "ι", "iota greek"),
    (r"\kappa", "κ", "kappa greek"),
    (r"\lambda", "λ", "lambda greek wavelength"),
    (r"\mu", "μ", "mu greek micro mean"),
    (r"\nu", "ν", "nu greek frequency"),
    (r"\xi", "ξ", "xi greek"),
    (r"\pi", "π", "pi greek"),
    (r"\varpi", "ϖ", "varpi pi greek"),
    (r"\rho", "ρ", "rho greek density"),
    (r"\varrho", "ϱ", "varrho rho greek"),
    (r"\sigma", "σ", "sigma greek standard deviation"),
    (r"\varsigma", "ς", "varsigma sigma greek"),
    (r"\tau", "τ", "tau greek torque"),
    (r"\upsilon", "υ", "upsilon greek"),
    (r"\phi", "ϕ", "phi greek"),
    (r"\varphi", "φ", "varphi phi greek"),
    (r"\chi", "χ", "chi greek"),
    (r"\psi", "ψ", "psi greek"),
    (r"\omega", "ω", "omega greek angular"),
    // Greek, upper case.
    (r"\Gamma", "Γ", "gamma capital greek"),
    (r"\Delta", "Δ", "delta capital greek change"),
    (r"\Theta", "Θ", "theta capital greek"),
    (r"\Lambda", "Λ", "lambda capital greek"),
    (r"\Xi", "Ξ", "xi capital greek"),
    (r"\Pi", "Π", "pi capital greek product"),
    (r"\Sigma", "Σ", "sigma capital greek sum"),
    (r"\Upsilon", "Υ", "upsilon capital greek"),
    (r"\Phi", "Φ", "phi capital greek"),
    (r"\Psi", "Ψ", "psi capital greek"),
    (r"\Omega", "Ω", "omega capital greek ohm"),
    // Hebrew and letter-like.
    (r"\aleph", "ℵ", "aleph hebrew cardinal"),
    (r"\hbar", "ℏ", "hbar planck reduced"),
    (r"\ell", "ℓ", "ell script l"),
    (r"\Re", "ℜ", "real part re"),
    (r"\Im", "ℑ", "imaginary part im"),
    (r"\wp", "℘", "weierstrass p"),
    (r"\infty", "∞", "infinity infinite"),
    (r"\partial", "∂", "partial derivative"),
    (r"\nabla", "∇", "nabla del gradient"),
    (r"\emptyset", "∅", "empty set emptyset null"),
    (r"\varnothing", "∅", "varnothing empty set"),
    (r"\degree", "°", "degree degrees"),
    (r"\angle", "∠", "angle"),
    (r"\triangle", "△", "triangle"),
    (r"\square", "□", "square box"),
    (r"\checkmark", "✓", "checkmark tick check"),
    (r"\dagger", "†", "dagger"),
    (r"\star", "⋆", "star"),
    // Number sets and fonts.
    (r"\mathbb{R}", "ℝ", "real numbers reals set mathbb"),
    (r"\mathbb{N}", "ℕ", "natural numbers naturals set mathbb"),
    (r"\mathbb{Z}", "ℤ", "integers set mathbb"),
    (r"\mathbb{Q}", "ℚ", "rational numbers rationals set mathbb"),
    (r"\mathbb{C}", "ℂ", "complex numbers set mathbb"),
    (r"\mathbb{P}", "ℙ", "probability primes mathbb"),
    (r"\mathbb{E}", "𝔼", "expectation expected value mathbb"),
    (r"\mathcal{L}", "ℒ", "mathcal calligraphic script letter laplace lagrangian"),
    (r"\mathcal{O}", "𝒪", "big o notation mathcal complexity"),
    (r"\mathrm{d}", "d", "mathrm upright roman differential"),
    (r"\mathbf{x}", "𝐱", "mathbf bold vector"),
    (r"\boldsymbol{\alpha}", "𝛂", "boldsymbol bold greek"),
    (r"\text{text}", "T", "text words plain"),
    (r"\operatorname{name}", "op", "operatorname custom function"),
    // Binary operators.
    (r"\times", "×", "times multiply cross product"),
    (r"\cdot", "·", "cdot dot multiply product"),
    (r"\div", "÷", "divide division"),
    (r"\pm", "±", "plus minus plusminus"),
    (r"\mp", "∓", "minus plus"),
    (r"\ast", "∗", "asterisk ast convolution"),
    (r"\circ", "∘", "circ composition compose ring"),
    (r"\bullet", "•", "bullet"),
    (r"\oplus", "⊕", "oplus xor direct sum"),
    (r"\otimes", "⊗", "otimes tensor product"),
    (r"\odot", "⊙", "odot hadamard"),
    (r"\wedge", "∧", "wedge and logical"),
    (r"\vee", "∨", "vee or logical"),
    (r"\setminus", "∖", "setminus difference minus"),
    (r"\cap", "∩", "cap intersection"),
    (r"\cup", "∪", "cup union"),
    (r"\sqcup", "⊔", "sqcup disjoint union"),
    // Relations.
    (r"\leq", "≤", "less than or equal leq le"),
    (r"\geq", "≥", "greater than or equal geq ge"),
    (r"\neq", "≠", "not equal neq ne"),
    (r"\approx", "≈", "approximately approx"),
    (r"\equiv", "≡", "equivalent equiv congruent identical"),
    (r"\sim", "∼", "sim similar distributed tilde"),
    (r"\simeq", "≃", "simeq similar equal"),
    (r"\cong", "≅", "congruent cong isomorphic"),
    (r"\propto", "∝", "proportional propto"),
    (r"\ll", "≪", "much less than ll"),
    (r"\gg", "≫", "much greater than gg"),
    (r"\prec", "≺", "precedes prec"),
    (r"\succ", "≻", "succeeds succ"),
    (r"\perp", "⊥", "perpendicular perp orthogonal"),
    (r"\parallel", "∥", "parallel"),
    (r"\mid", "∣", "mid divides given"),
    (r"\nmid", "∤", "does not divide nmid"),
    (r"\coloneqq", "≔", "defined as coloneqq definition assign"),
    (r"\doteq", "≐", "doteq approaches"),
    // Sets and logic.
    (r"\in", "∈", "in element of member"),
    (r"\notin", "∉", "not in notin element"),
    (r"\ni", "∋", "ni contains member"),
    (r"\subset", "⊂", "subset"),
    (r"\subseteq", "⊆", "subseteq subset equal"),
    (r"\supset", "⊃", "superset supset"),
    (r"\supseteq", "⊇", "supseteq superset equal"),
    (r"\forall", "∀", "for all forall every"),
    (r"\exists", "∃", "exists there exists"),
    (r"\nexists", "∄", "not exists nexists"),
    (r"\neg", "¬", "not negation neg lnot"),
    (r"\land", "∧", "land logical and"),
    (r"\lor", "∨", "lor logical or"),
    (r"\top", "⊤", "top true tautology"),
    (r"\bot", "⊥", "bot false contradiction bottom"),
    (r"\vdash", "⊢", "vdash proves turnstile"),
    (r"\models", "⊨", "models entails"),
    (r"\therefore", "∴", "therefore"),
    (r"\because", "∵", "because"),
    // Arrows.
    (r"\to", "→", "to right arrow maps limit"),
    (r"\rightarrow", "→", "rightarrow right arrow"),
    (r"\leftarrow", "←", "leftarrow left arrow gets"),
    (r"\leftrightarrow", "↔", "leftrightarrow both arrow"),
    (r"\Rightarrow", "⇒", "implies rightarrow double arrow"),
    (r"\Leftarrow", "⇐", "implied by leftarrow double arrow"),
    (r"\Leftrightarrow", "⇔", "iff if and only if double arrow equivalence"),
    (r"\implies", "⟹", "implies"),
    (r"\iff", "⟺", "iff if and only if"),
    (r"\mapsto", "↦", "mapsto maps to function"),
    (r"\uparrow", "↑", "uparrow up arrow"),
    (r"\downarrow", "↓", "downarrow down arrow"),
    (r"\updownarrow", "↕", "updownarrow up down arrow"),
    (r"\nearrow", "↗", "nearrow north east arrow"),
    (r"\searrow", "↘", "searrow south east arrow"),
    (r"\hookrightarrow", "↪", "hookrightarrow injection embedding arrow"),
    (r"\rightleftharpoons", "⇌", "rightleftharpoons equilibrium reversible reaction"),
    (r"\xrightarrow{f}", "→ᶠ", "xrightarrow labelled arrow over"),
    (r"\longrightarrow", "⟶", "longrightarrow long arrow"),
    // Big operators and calculus.
    (r"\sum_{i=1}^{n}", "∑", "sum summation sigma"),
    (r"\prod_{i=1}^{n}", "∏", "product prod"),
    (r"\coprod", "∐", "coproduct coprod"),
    (r"\int", "∫", "integral int"),
    (r"\int_{a}^{b}", "∫ₐᵇ", "definite integral bounds limits"),
    (r"\iint", "∬", "double integral iint"),
    (r"\iiint", "∭", "triple integral iiint"),
    (r"\oint", "∮", "contour integral oint closed line"),
    (r"\bigcup_{i}", "⋃", "big union bigcup"),
    (r"\bigcap_{i}", "⋂", "big intersection bigcap"),
    (r"\lim_{x \to \infty}", "lim", "limit lim infinity"),
    (r"\limsup", "lim sup", "limsup limit superior"),
    (r"\liminf", "lim inf", "liminf limit inferior"),
    (r"\frac{d}{dx}", "d/dx", "derivative frac d dx differentiate"),
    (r"\frac{\partial f}{\partial x}", "∂f/∂x", "partial derivative frac"),
    (r"\,dx", "dx", "dx differential spacing integral"),
    (r"\max", "max", "max maximum"),
    (r"\min", "min", "min minimum"),
    (r"\sup", "sup", "sup supremum"),
    (r"\inf", "inf", "inf infimum"),
    (r"\arg\max", "argmax", "argmax arg max"),
    (r"\log", "log", "log logarithm"),
    (r"\ln", "ln", "ln natural log logarithm"),
    (r"\exp", "exp", "exp exponential"),
    (r"\sin", "sin", "sin sine trig"),
    (r"\cos", "cos", "cos cosine trig"),
    (r"\tan", "tan", "tan tangent trig"),
    (r"\det", "det", "det determinant"),
    (r"\gcd", "gcd", "gcd greatest common divisor"),
    (r"\bmod", "mod", "bmod modulo mod"),
    (r"\pmod{n}", "(mod n)", "pmod modulo congruence"),
    // Structures.
    (r"\frac{a}{b}", "a⁄b", "fraction frac divide over"),
    (r"\dfrac{a}{b}", "a⁄b", "dfrac display fraction"),
    (r"\tfrac{a}{b}", "a⁄b", "tfrac text fraction small"),
    (r"\sqrt{x}", "√", "square root sqrt radical"),
    (r"\sqrt[n]{x}", "ⁿ√", "nth root sqrt radical cube"),
    (r"x^{n}", "xⁿ", "power superscript exponent"),
    (r"x_{i}", "xᵢ", "subscript index"),
    (r"\binom{n}{k}", "(ⁿₖ)", "binomial binom choose combination"),
    (r"\overline{x}", "x̄", "overline bar mean conjugate"),
    (r"\underline{x}", "x̲", "underline"),
    (r"\bar{x}", "x̄", "bar mean average"),
    (r"\hat{x}", "x̂", "hat estimator unit vector"),
    (r"\tilde{x}", "x̃", "tilde"),
    (r"\vec{v}", "v⃗", "vector arrow vec"),
    (r"\dot{x}", "ẋ", "dot time derivative"),
    (r"\ddot{x}", "ẍ", "ddot second derivative"),
    (r"\widehat{xy}", "x̂y", "widehat wide hat"),
    (r"\overbrace{a+b}^{n}", "⏞", "overbrace brace"),
    (r"\underbrace{a+b}_{n}", "⏟", "underbrace brace"),
    (r"\left( \right)", "( )", "left right parentheses brackets auto size"),
    (r"\left[ \right]", "[ ]", "left right square brackets"),
    (r"\left\{ \right\}", "{ }", "left right curly braces set"),
    (r"\langle \rangle", "⟨⟩", "angle brackets langle rangle inner product"),
    (r"\lvert x \rvert", "|x|", "absolute value abs lvert rvert"),
    (r"\lVert x \rVert", "‖x‖", "norm lvert double bars"),
    (r"\lfloor x \rfloor", "⌊x⌋", "floor"),
    (r"\lceil x \rceil", "⌈x⌉", "ceiling ceil"),
    (r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}", "(▦)", "matrix pmatrix parentheses"),
    (r"\begin{bmatrix} a & b \\ c & d \end{bmatrix}", "[▦]", "matrix bmatrix brackets"),
    (r"\begin{vmatrix} a & b \\ c & d \end{vmatrix}", "|▦|", "determinant vmatrix matrix"),
    (r"\begin{cases} a & x > 0 \\ b & x \le 0 \end{cases}", "{", "cases piecewise function"),
    (r"\begin{aligned} a &= b \\ c &= d \end{aligned}", "≡", "aligned align equations multiline"),
    (r"\cdots", "⋯", "cdots ellipsis dots center"),
    (r"\ldots", "…", "ldots ellipsis dots"),
    (r"\vdots", "⋮", "vdots vertical dots"),
    (r"\ddots", "⋱", "ddots diagonal dots"),
    (r"\quad", "␣", "quad space spacing"),
    (r"\qquad", "␣␣", "qquad double space spacing"),
    (r"\boxed{x}", "▢", "boxed box answer"),
    (r"\cancel{x}", "x̸", "cancel strike"),
    (r"\color{red}{x}", "🖍", "color colour red"),
];

/// The query part after a LaTeX trigger ("\", "latex ", "tex "), if any.
pub fn query(raw: &str) -> Option<&str> {
    let t = raw.trim_start();
    if let Some(rest) = t.strip_prefix('\\') {
        return Some(rest);
    }
    let lower = t.get(..6).map(str::to_ascii_lowercase);
    if lower.as_deref() == Some("latex ") {
        return Some(&t[6..]);
    }
    if t.get(..4).map(str::to_ascii_lowercase).as_deref() == Some("tex ") {
        return Some(&t[4..]);
    }
    None
}

/// Best matches for `q` (without the trigger), most relevant first.
pub fn search(q: &str, limit: usize) -> Vec<(&'static str, &'static str, &'static str)> {
    let q = q.trim();
    if q.is_empty() {
        return SYMBOLS.iter().take(limit).copied().collect();
    }
    let ql = q.to_lowercase();
    let mut scored: Vec<(i32, usize)> = SYMBOLS
        .iter()
        .enumerate()
        .filter_map(|(i, (code, _, words))| {
            let name = code.trim_start_matches('\\').split(['{', '_', '^', '[', ' ']).next().unwrap_or("");
            // Case matters for the name itself (\phi vs \Phi).
            let exact = if name == q { 2000 } else if name.eq_ignore_ascii_case(q) { 1500 } else { 0 };
            let by_name = super::index::score_text(name, &ql).map(|s| s + 100);
            let by_words = words.split(' ').filter_map(|w| super::index::score_text(w, &ql)).max();
            let s = [Some(exact).filter(|e| *e > 0), by_name, by_words].into_iter().flatten().max()?;
            Some((s, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, i)| SYMBOLS[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers() {
        assert_eq!(query(r"\alp"), Some("alp"));
        assert_eq!(query("latex arrow"), Some("arrow"));
        assert_eq!(query("Tex sum"), Some("sum"));
        assert_eq!(query("texture"), None);
        assert_eq!(query("alpha"), None);
    }

    #[test]
    fn finds_commands() {
        assert_eq!(search("alpha", 1)[0].0, r"\alpha");
        assert_eq!(search("Phi", 1)[0].0, r"\Phi");
        assert_eq!(search("phi", 1)[0].0, r"\phi");
        assert_eq!(search("int", 1)[0].0, r"\int");
        assert!(search("arrow", 10).iter().any(|s| s.0 == r"\rightarrow"));
        assert!(search("fraction", 3).iter().any(|s| s.0 == r"\frac{a}{b}"));
    }
}
