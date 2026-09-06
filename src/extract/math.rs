//! Math extraction: HTML `<math>` (MathML) elements → LaTeX-ish
//! inline text. Technical pages (Wikipedia ML/physics/math, papers,
//! docs) carry their formulas as MathML with the ORIGINAL LaTeX in
//! `alttext` (MediaWiki) or `<annotation encoding="application/x-tex">`.
//! Dropping these elements guts the page's core information : the
//! v2.1 behavior. This module recovers the formula, preferring the
//! true LaTeX and falling back to a compact MathML serialization.

use scraper::ElementRef;

/// MathML element names (rendered structure, not content).
fn is_mathml_tag(name: &str) -> bool {
    matches!(
        name,
        "math"
            | "semantics"
            | "annotation"
            | "annotation-xml"
            | "mrow"
            | "mi"
            | "mn"
            | "mo"
            | "ms"
            | "mtext"
            | "mspace"
            | "mpadded"
            | "mstyle"
            | "maction"
            | "menclose"
            | "mphantom"
            // Layout/script tags: each has its own serialize_at arm,
            // but the child-recursion guard consults this list too :
            // missing entries made e.g. an msub inside an mtd (both
            // absent) invisible, so matrix cells serialized empty.
            | "mfrac"
            | "msqrt"
            | "mroot"
            | "msub"
            | "msup"
            | "msubsup"
            | "munder"
            | "mover"
            | "munderover"
            | "mmultiscripts"
            | "mprescripts"
            | "none"
            | "mtable"
            | "mtr"
            | "mlabeledtr"
            | "mtd"
            | "merror"
            | "mfenced"
            | "mglyph"
    )
}

/// Best LaTeX for a `<math>` element:
/// 1. `alttext` attribute (MediaWiki ships the original LaTeX),
/// 2. `<annotation encoding="application/x-tex">` child,
/// 3. compact serialization of the MathML tree.
pub fn latex(el: ElementRef<'_>) -> String {
    if let Some(alt) = el.value().attr("alttext")
        && !alt.trim().is_empty()
    {
        return strip_displaystyle(alt.trim());
    }
    if let Some(a) = tex_annotation(el) {
        return a;
    }
    let s = serialize(el);
    s.trim().to_string()
}

/// MediaWiki alttext wraps content as `{\displaystyle ...}` (or
/// `\displaystyle ...`) : rendering boilerplate, not math. Strip
/// to the clean formula the agent actually wants to read.
fn strip_displaystyle(s: &str) -> String {
    let mut t = s.trim();
    if let Some(rest) = t.strip_prefix("{\\displaystyle ") {
        // Strip exactly ONE closing brace : trim_end_matches would
        // eat inner braces too ("W_{Q}}" → "W_{Q").
        t = rest.strip_suffix('}').unwrap_or(rest).trim();
    } else if let Some(rest) = t.strip_prefix("\\displaystyle ") {
        t = rest.trim();
    }
    t.to_string()
}

/// `<annotation encoding="application/x-tex">` child content.
fn tex_annotation(el: ElementRef<'_>) -> Option<String> {
    for enc in ["application/x-tex", "application/latex"] {
        let sel = scraper::Selector::parse(&format!("annotation[encoding=\"{enc}\"]")).ok()?;
        if let Some(a) = el.select(&sel).next() {
            let t: String = a.text().collect::<Vec<_>>().join("");
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Recursion cap. Every other DOM walker has one (blocks 300, inline
/// 100, score 300); this one had none, and MathML nests as deep as a
/// page cares to make it: a couple of thousand nested <mrow> in a
/// release build (a 10-20 KB page; a few hundred in debug) overflowed
/// the 2 MB tokio worker stack, which is a hard abort of the daemon,
/// not a panic. Past the cap the subtree degrades to its plain text,
/// an iterative tree walk. Legitimate MathML nests ~20-40 deep.
const MAX_DEPTH: usize = 100;

/// Compact linear serialization of a MathML subtree. Not full
/// LaTeX : a token-efficient linearization an LLM reads natively:
/// `W_Q^T`, `(Q K^T)/(sqrt(d_k))`, matrices as `(a, b; c, d)`.
fn serialize(el: ElementRef<'_>) -> String {
    serialize_at(el, 0)
}

fn serialize_at(el: ElementRef<'_>, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return descendants_text(el);
    }
    let name = el.value().name();
    match name {
        // Scripted constructs: gather children positionally.
        "msup" | "msub" | "msubsup" | "munder" | "mover" | "munderover" | "mroot"
        | "mmultiscripts" => serialize_scripted(el, name, depth),
        "mfrac" => {
            let kids = element_children(el);
            if kids.len() == 2 {
                format!(
                    "({})/({})",
                    serialize_at(kids[0], depth + 1),
                    serialize_at(kids[1], depth + 1)
                )
            } else {
                kids.iter()
                    .map(|k| serialize_at(*k, depth + 1))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
        "msqrt" => {
            let inner: String = element_children(el)
                .iter()
                .map(|k| serialize_at(*k, depth + 1))
                .collect::<Vec<_>>()
                .join("");
            format!("sqrt({inner})")
        }
        "mtable" => {
            // Matrix: rows → "; ", cells → ", ".
            let mut rows = Vec::new();
            for tr in el
                .select(&scraper::Selector::parse("mtr").unwrap())
                .take(12)
            {
                let cells: Vec<String> = tr
                    .select(&scraper::Selector::parse("mtd").unwrap())
                    .take(12)
                    .map(|c| serialize_at(c, depth + 1))
                    .collect();
                if !cells.is_empty() {
                    rows.push(cells.join(", "));
                }
            }
            if rows.is_empty() {
                descendants_text(el)
            } else {
                format!("({})", rows.join("; "))
            }
        }
        "annotation" | "annotation-xml" => String::new(), // handled by latex(); never leak as prose
        "mspace" => " ".into(),
        // Containers and tokens: concatenate serialized children :
        // `mrow(mi W, mo _, mi Q)` → "W_Q" via the sub/sup rules.
        _ => {
            let mut out = String::new();
            for child in el.children() {
                match child.value() {
                    scraper::Node::Text(t) => {
                        let s = t.text.trim();
                        if !s.is_empty() {
                            out.push_str(s);
                        }
                    }
                    scraper::Node::Element(_) => {
                        if let Some(c) = ElementRef::wrap(child) {
                            let cname = c.value().name();
                            if is_mathml_tag(cname) || is_mathml_tag(name) {
                                out.push_str(&serialize_at(c, depth + 1));
                            }
                        }
                    }
                    _ => {}
                }
            }
            out
        }
    }
}

/// Serialize msup/msub/msubsup/munder/mover/munderover/mroot with
/// positional children: base, sub, sup.
fn serialize_scripted(el: ElementRef<'_>, name: &str, depth: usize) -> String {
    let kids = element_children(el);
    let kid = |i: usize| kids.get(i).map(|k| serialize_at(*k, depth + 1));
    let base = kid(0).unwrap_or_default();
    let (sub, sup) = match name {
        "msub" => (kid(1), None),
        "msup" => (None, kid(1)),
        "msubsup" | "munderover" => (kid(1), kid(2)),
        "munder" => (kid(1), None),
        "mover" => (None, kid(1)),
        "mroot" => (None, kid(1)), // base^(index)
        _ => (None, None),
    };
    let mut out = base;
    if let Some(s) = sub
        && !s.is_empty()
    {
        out.push_str(&format!("_{{{s}}}"));
    }
    if let Some(s) = sup
        && !s.is_empty()
    {
        out.push_str(&format!("^{{{s}}}"));
    }
    out
}

/// Direct element children (skipping whitespace text nodes).
fn element_children(el: ElementRef<'_>) -> Vec<ElementRef<'_>> {
    el.children().filter_map(ElementRef::wrap).collect()
}

/// All descendant text, whitespace-collapsed (fallback for
/// exotic MathML).
fn descendants_text(el: ElementRef<'_>) -> String {
    let s: String = el.text().collect::<Vec<_>>().join("");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn math_el(html: &str) -> ElementRef<'static> {
        let doc = Box::leak(Box::new(scraper::Html::parse_document(html)));
        doc.select(&scraper::Selector::parse("math").unwrap())
            .next()
            .expect("math element")
    }

    // Unbounded recursion over page-controlled nesting is a stack
    // overflow -- a hard abort of the daemon, not a panic -- and
    // html5ever happily builds a 20000-deep <mrow> tree from a
    // ~250 KB page. Run on a deliberately small stack: this only
    // passes when the walk is depth-capped (100 frames fit in 2 MiB
    // with room to spare; 20000 do not on any build profile).
    #[test]
    fn deeply_nested_mathml_does_not_overflow_the_stack() {
        const DEPTH: usize = 20_000;
        let html = format!(
            "<math>{}<mi>x</mi>{}</math>",
            "<mrow>".repeat(DEPTH),
            "</mrow>".repeat(DEPTH)
        );
        let out = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || latex(math_el(&html)))
            .expect("spawn")
            .join()
            .expect("serialize must not overflow");
        assert!(out.contains('x'), "{out}");
    }

    // The cap degrades to plain text, it does not drop content.
    #[test]
    fn past_the_depth_cap_content_is_kept_as_text() {
        let depth = MAX_DEPTH + 50;
        let html = format!(
            "<math>{}<mi>deep</mi>{}</math>",
            "<mrow>".repeat(depth),
            "</mrow>".repeat(depth)
        );
        assert!(latex(math_el(&html)).contains("deep"));
    }

    #[test]
    fn displaystyle_wrapper_strips_exactly_one_brace() {
        // "{\displaystyle W_{Q}}" → "W_{Q}" : never "W_{Q" (the
        // trim_end_matches bug ate inner braces).
        let el = math_el(r#"<math alttext="{\displaystyle W_{Q}}"><mi>x</mi></math>"#);
        assert_eq!(latex(el), "W_{Q}");
        // Content whose last token legitimately ends in braces:
        let el2 = math_el(r#"<math alttext="{\displaystyle W_{Q}+W_{K}}"><mi>x</mi></math>"#);
        assert_eq!(latex(el2), "W_{Q}+W_{K}");
    }

    #[test]
    fn alttext_wins() {
        // MediaWiki shape: alttext carries the original LaTeX.
        let el = math_el(
            r#"<math alttext="\mathrm{Attention}(Q,K,V)=\mathrm{softmax}\left(\frac{QK^T}{\sqrt{d_k}}\right)"><semantics><mrow>ignored</mrow></semantics></math>"#,
        );
        let l = latex(el);
        assert!(l.contains("Attention"), "{l}");
        assert!(l.contains("d_k"), "{l}");
    }

    #[test]
    fn tex_annotation_second() {
        let el = math_el(
            r#"<math><semantics><mrow><mi>x</mi></mrow><annotation encoding="application/x-tex">x^2 + 1</annotation></semantics></math>"#,
        );
        assert_eq!(latex(el), "x^2 + 1");
    }

    #[test]
    fn serializes_sub_sup() {
        // W_Q^T without alttext: msubsup(W, Q, T)
        let el = math_el(
            r#"<math><mrow><msubsup><mi>W</mi><mi>Q</mi><mi>T</mi></msubsup></mrow></math>"#,
        );
        let l = latex(el);
        assert!(l.contains("W"), "{l}");
        assert!(l.contains("_{Q}"), "{l}");
        assert!(l.contains("^{T}"), "{l}");
    }

    #[test]
    fn serializes_frac_and_sqrt() {
        let el = math_el(
            r#"<math><mfrac><mrow><mi>Q</mi><msup><mi>K</mi><mi>T</mi></msup></mrow><msqrt><msub><mi>d</mi><mi>k</mi></msub></msqrt></mfrac></math>"#,
        );
        let l = latex(el);
        assert!(l.contains(")/("), "frac shape: {l}");
        assert!(l.contains("sqrt("), "{l}");
        assert!(l.contains("^{T}"), "{l}");
    }

    #[test]
    fn serializes_matrix() {
        let el = math_el(
            r#"<math><mtable><mtr><mtd><mn>1</mn></mtd><mtd><mn>2</mn></mtd></mtr><mtr><mtd><mn>3</mn></mtd><mtd><mn>4</mn></mtd></mtr></mtable></math>"#,
        );
        let l = latex(el);
        assert!(l.contains("(1, 2; 3, 4)"), "{l}");
    }

    // The child-recursion guard only follows a child when the child
    // OR its parent is a known MathML tag : with the layout/script
    // tags (mtd, mfrac, msub, ...) missing from the list, a scripted
    // construct inside a table cell was invisible on both sides of
    // the check and every such cell serialized empty.
    #[test]
    fn serializes_scripted_constructs_inside_matrix_cells() {
        let el = math_el(
            r#"<math><mtable><mtr><mtd><msub><mi>a</mi><mn>1</mn></msub></mtd><mtd><mfrac><mn>1</mn><mn>2</mn></mfrac></mtd></mtr></mtable></math>"#,
        );
        let l = latex(el);
        assert!(l.contains("a_{1}"), "msub cell dropped: {l}");
        assert!(l.contains("(1)/(2)"), "mfrac cell dropped: {l}");
    }

    #[test]
    fn annotation_never_leaks_as_prose() {
        // serialize() must not include annotation content when
        // serializing the semantics container.
        let el = math_el(
            r#"<math><semantics><mrow><mi>z</mi></mrow><annotation encoding="application/x-tex">z</annotation></semantics></math>"#,
        );
        let l = latex(el);
        assert_eq!(l, "z");
    }
}
