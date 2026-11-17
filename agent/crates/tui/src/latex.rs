//! LaTeX rendering support.
//!
//! Provides basic LaTeX rendering for terminal output.

use regex::Regex;

/// Options for LaTeX rendering.
#[derive(Debug, Clone)]
pub struct RenderLatexOptions {
    /// Maximum width in columns.
    pub max_width: usize,
    /// Whether to use Unicode symbols.
    pub use_unicode: bool,
    /// Whether to use fallback ASCII.
    pub fallback_ascii: bool,
}

impl Default for RenderLatexOptions {
    fn default() -> Self {
        Self {
            max_width: 80,
            use_unicode: true,
            fallback_ascii: true,
        }
    }
}

/// Common LaTeX symbols and their Unicode equivalents.
const LATEX_SYMBOLS: &[(&str, &str)] = &[
    // Greek letters
    ("\\alpha", "α"),
    ("\\beta", "β"),
    ("\\gamma", "γ"),
    ("\\delta", "δ"),
    ("\\epsilon", "ε"),
    ("\\zeta", "ζ"),
    ("\\eta", "η"),
    ("\\theta", "θ"),
    ("\\iota", "ι"),
    ("\\kappa", "κ"),
    ("\\lambda", "λ"),
    ("\\mu", "μ"),
    ("\\nu", "ν"),
    ("\\xi", "ξ"),
    ("\\pi", "π"),
    ("\\rho", "ρ"),
    ("\\sigma", "σ"),
    ("\\tau", "τ"),
    ("\\upsilon", "υ"),
    ("\\phi", "φ"),
    ("\\chi", "χ"),
    ("\\psi", "ψ"),
    ("\\omega", "ω"),
    // Capital Greek
    ("\\Gamma", "Γ"),
    ("\\Delta", "Δ"),
    ("\\Theta", "Θ"),
    ("\\Lambda", "Λ"),
    ("\\Xi", "Ξ"),
    ("\\Pi", "Π"),
    ("\\Sigma", "Σ"),
    ("\\Phi", "Φ"),
    ("\\Psi", "Ψ"),
    ("\\Omega", "Ω"),
    // Math operators
    ("\\pm", "±"),
    ("\\mp", "∓"),
    ("\\times", "×"),
    ("\\div", "÷"),
    ("\\cdot", "·"),
    ("\\ast", "∗"),
    ("\\star", "⋆"),
    ("\\circ", "∘"),
    ("\\bullet", "•"),
    ("\\oplus", "⊕"),
    ("\\ominus", "⊖"),
    ("\\otimes", "⊗"),
    ("\\oslash", "⊘"),
    ("\\odot", "⊙"),
    // Relations
    ("\\leq", "≤"),
    ("\\geq", "≥"),
    ("\\neq", "≠"),
    ("\\approx", "≈"),
    ("\\equiv", "≡"),
    ("\\sim", "∼"),
    ("\\propto", "∝"),
    ("\\infty", "∞"),
    ("\\partial", "∂"),
    ("\\nabla", "∇"),
    ("\\forall", "∀"),
    ("\\exists", "∃"),
    ("\\in", "∈"),
    ("\\notin", "∉"),
    ("\\subset", "⊂"),
    ("\\supset", "⊃"),
    ("\\subseteq", "⊆"),
    ("\\supseteq", "⊇"),
    ("\\cup", "∪"),
    ("\\cap", "∩"),
    ("\\emptyset", "∅"),
    // Arrows
    ("\\rightarrow", "→"),
    ("\\leftarrow", "←"),
    ("\\uparrow", "↑"),
    ("\\downarrow", "↓"),
    ("\\Rightarrow", "⇒"),
    ("\\Leftarrow", "⇐"),
    ("\\Uparrow", "⇑"),
    ("\\Downarrow", "⇓"),
    ("\\leftrightarrow", "↔"),
    ("\\Leftrightarrow", "⇔"),
    // Misc
    ("\\degree", "°"),
    ("\\prime", "′"),
    ("\\dagger", "†"),
    ("\\ddagger", "‡"),
    ("\\S", "§"),
    ("\\P", "¶"),
    ("\\copyright", "©"),
    ("\\registered", "®"),
    ("\\trademark", "™"),
];

/// Common LaTeX commands that need special handling.
const LATEX_COMMANDS: &[(&str, &str)] = &[
    ("\\textbf{", "**"),
    ("\\textit{", "*"),
    ("\\emph{", "*"),
    ("\\underline{", "_"),
    ("\\text{", ""),
    ("\\mathrm{", ""),
    ("\\mathbf{", "**"),
    ("\\mathit{", "*"),
    ("\\mathcal{", ""),
    ("\\mathbb{", ""),
    ("\\mathfrak{", ""),
];

/// Render LaTeX to Unicode text.
pub fn render_latex(latex: &str, options: &RenderLatexOptions) -> String {
    let mut result = latex.to_string();

    // Remove $ delimiters
    result = result.replace('$', "");

    // Remove \[ and \] delimiters
    result = result.replace("\\[", "").replace("\\]", "");

    // Remove \( and \) delimiters
    result = result.replace("\\(", "").replace("\\)", "");

    // Handle subscripts and superscripts
    result = render_scripts(&result, options);

    // Handle fractions
    result = render_fractions(&result, options);

    // Replace symbols
    for (latex, unicode) in LATEX_SYMBOLS {
        result = result.replace(latex, unicode);
    }

    // Handle text commands
    for (cmd, replacement) in LATEX_COMMANDS {
        while let Some(start) = result.find(cmd) {
            if let Some(end) = find_matching_brace(&result, start + cmd.len() - 1) {
                let content = &result[start + cmd.len()..end];
                let replacement_text = if replacement.is_empty() {
                    content.to_string()
                } else {
                    format!("{}{}{}", replacement, content, replacement)
                };
                result = format!(
                    "{}{}{}",
                    &result[..start],
                    replacement_text,
                    &result[end + 1..]
                );
            } else {
                break;
            }
        }
    }

    // Clean up remaining braces
    result = result.replace(['{', '}'], "");

    // Handle special LaTeX spaces
    result = result
        .replace("\\,", " ")
        .replace("\\:", " ")
        .replace("\\;", " ");
    result = result.replace("\\!", "").replace("\\ ", " ");

    // Handle quotes
    result = result.replace("``", "\"").replace("''", "\"");
    result = result.replace("`", "'").replace("'", "'");

    // Trim and limit width
    result = result.trim().to_string();
    if result.len() > options.max_width {
        result = format!("{}...", &result[..options.max_width.saturating_sub(3)]);
    }

    result
}

/// Render subscripts and superscripts.
fn render_scripts(latex: &str, options: &RenderLatexOptions) -> String {
    let mut result = latex.to_string();

    if !options.use_unicode {
        // Use ^ and _ notation
        return result;
    }

    // Unicode subscript and superscript characters
    let subscripts: Vec<char> = "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₐₑₕᵢⱼₖₗₘₙₒₚᵣₛₜᵤᵥₓ".chars().collect();
    let superscripts: Vec<char> = "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ᵃᵇᶜᵈᵉᶠᵍʰⁱʲᵏˡᵐⁿᵒᵖʳˢᵗᵘᵛʷˣʸᶻ".chars().collect();

    // Simple numeric subscripts: _0, _1, etc.
    let digits = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9'];
    for (i, &d) in digits.iter().enumerate() {
        result = result.replace(&format!("_{}", d), &subscripts[i].to_string());
        result = result.replace(&format!("^{}", d), &superscripts[i].to_string());
    }

    result
}

/// Render simple fractions.
fn render_fractions(latex: &str, _options: &RenderLatexOptions) -> String {
    let fraction_re = Regex::new(r"\\frac\{([^}]*)\}\{([^}]*)\}").unwrap();
    let mut result = latex.to_string();

    while let Some(caps) = fraction_re.captures(&result) {
        if let (Some(full), Some(num), Some(denom)) = (caps.get(0), caps.get(1), caps.get(2)) {
            // Use Unicode fraction slash
            let fraction = format!("{}⁄{}", num.as_str(), denom.as_str());
            result = format!(
                "{}{}{}",
                &result[..full.start()],
                fraction,
                &result[full.end()..]
            );
        } else {
            break;
        }
    }

    result
}

/// Find matching closing brace.
fn find_matching_brace(s: &str, start: usize) -> Option<usize> {
    let chars: Vec<char> = s.chars().collect();
    if start >= chars.len() || chars[start] != '{' {
        return None;
    }

    let mut depth = 1;
    let mut pos = start + 1;

    while pos < chars.len() && depth > 0 {
        match chars[pos] {
            '{' => depth += 1,
            '}' => depth -= 1,
            '\\' => {
                // Skip escaped character
                pos += 1;
            }
            _ => {}
        }
        pos += 1;
    }

    if depth == 0 {
        Some(pos - 1)
    } else {
        None
    }
}

/// Check if a string contains LaTeX.
pub fn is_latex(s: &str) -> bool {
    // Check for common LaTeX patterns
    s.contains('$')
        || s.contains("\\[")
        || s.contains("\\(")
        || s.contains("\\frac")
        || s.contains("\\begin")
        || LATEX_SYMBOLS.iter().any(|(sym, _)| s.contains(sym))
}

/// Strip LaTeX formatting.
pub fn strip_latex(latex: &str) -> String {
    let mut result = latex.to_string();

    // Remove $ delimiters
    result = result.replace('$', "");

    // Remove block delimiters
    result = result.replace("\\[", "").replace("\\]", "");
    result = result.replace("\\(", "").replace("\\)", "");

    // Remove commands, keeping content
    for (cmd, _) in LATEX_COMMANDS {
        while let Some(start) = result.find(cmd) {
            if let Some(end) = find_matching_brace(&result, start + cmd.len() - 1) {
                let content = &result[start + cmd.len()..end];
                result = format!("{}{}{}", &result[..start], content, &result[end + 1..]);
            } else {
                break;
            }
        }
    }

    // Remove symbols (no Unicode equivalent)
    for (sym, _) in LATEX_SYMBOLS {
        result = result.replace(sym, "");
    }

    // Clean up braces
    result = result.replace(['{', '}'], "");

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_latex_simple() {
        let options = RenderLatexOptions::default();
        let result = render_latex("$x^2$", &options);
        assert!(result.contains('²') || result.contains("x^2"));
    }

    #[test]
    fn test_render_latex_symbols() {
        let options = RenderLatexOptions::default();
        let result = render_latex("$\\alpha + \\beta$", &options);
        assert!(result.contains('α'));
        assert!(result.contains('β'));
    }

    #[test]
    fn test_render_latex_fraction() {
        let options = RenderLatexOptions::default();
        let result = render_latex("$\\frac{1}{2}$", &options);
        assert!(result.contains('1') && result.contains('2'));
    }

    #[test]
    fn test_is_latex() {
        assert!(is_latex("$x^2$"));
        assert!(is_latex("\\alpha"));
        assert!(!is_latex("plain text"));
    }

    #[test]
    fn test_strip_latex() {
        let result = strip_latex("\\textbf{bold}");
        assert_eq!(result, "bold");
    }
}
