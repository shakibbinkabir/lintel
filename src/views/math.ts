// LaTeX in a chat answer, drawn as native MathML (the webview does the typesetting;
// no library). Covers what answers use: letters, numbers, operators, ^ and _, \frac,
// \sqrt, groups, \left \right, \text, Greek letters and the usual symbols. Anything
// else throws, and the caller shows the source as code instead of a wrong formula.

const NS = "http://www.w3.org/1998/Math/MathML";

const el = (tag: string, ...kids: Node[]): Element => {
  const e = document.createElementNS(NS, tag);
  e.append(...kids);
  return e;
};
const leaf = (tag: string, text: string): Element => {
  const e = document.createElementNS(NS, tag);
  e.textContent = text;
  return e;
};

const LETTERS = "alpha β beta γ gamma δ delta ε epsilon ζ zeta η eta θ theta λ lambda μ mu ν nu ξ xi π pi ρ rho σ sigma τ tau φ phi χ chi ψ psi ω omega Γ Gamma Δ Delta Θ Theta Λ Lambda Ξ Xi Π Pi Σ Sigma Φ Phi Ψ Psi Ω Omega ∂ partial ∇ nabla ℓ ell ℏ hbar ∞ infty";
const OPS = "± pm ∓ mp × times ÷ div · cdot ∗ ast ≤ leq ≤ le ≥ geq ≥ ge ≠ neq ≠ ne ≈ approx ≡ equiv ∼ sim ∝ propto ≪ ll ≫ gg → to → rightarrow ← leftarrow ⇒ Rightarrow ⇐ Leftarrow ⇔ Leftrightarrow ↔ leftrightarrow ↦ mapsto ∈ in ∉ notin ⊂ subset ⊆ subseteq ∪ cup ∩ cap ∀ forall ∃ exists ¬ neg ∧ wedge ∨ vee … ldots ⋯ cdots ∑ sum ∏ prod ∫ int ∮ oint ′ prime ° circ ∅ emptyset";

function table(list: string): Map<string, string> {
  const words = list.split(" ");
  const map = new Map<string, string>();
  for (let i = 0; i + 1 < words.length; i += 2) map.set(words[i + 1], words[i]);
  return map;
}
// "alpha β beta ..." starts with a name, so the first pair is read the other way round.
const GREEK = table("α " + LETTERS);
const SYMBOLS = table(OPS);
const LARGE = new Set(["sum", "prod", "int", "oint"]);
const FUNCTIONS = new Set(["sin", "cos", "tan", "log", "ln", "exp", "lim", "min", "max", "det", "sup", "inf"]);
const SPACES = new Set([",", ";", ":", "!", " ", "quad", "qquad"]);

const TOKEN = /\\([a-zA-Z]+|.)|\d+(?:\.\d+)?|[{}^_]|[^\s]/g;

export function mathml(tex: string, display: boolean): Element {
  const tokens = tex.match(TOKEN) ?? [];
  let at = 0;

  const bad = (): never => {
    throw new Error("math");
  };

  function group(): Element {
    // After "{": everything up to the matching "}".
    const kids: Node[] = [];
    while (at < tokens.length && tokens[at] !== "}") kids.push(scripted());
    if (tokens[at++] !== "}") bad();
    return el("mrow", ...kids);
  }

  /** One argument: a {group} or a single token. */
  function arg(): Node {
    if (at >= tokens.length) bad();
    if (tokens[at] === "{") {
      at++;
      return group();
    }
    return atom();
  }

  function text(): string {
    // \text{…}: the raw tokens up to the closing brace, spaces kept simple.
    if (tokens[at++] !== "{") bad();
    let out = "";
    while (at < tokens.length && tokens[at] !== "}") out += tokens[at++].replace(/^\\/, "");
    at++;
    return out;
  }

  function atom(): Node {
    const tok = tokens[at++];
    if (tok === undefined || tok === "}" || tok === "^" || tok === "_") return bad();
    if (tok === "{") return group();
    if (/^\d/.test(tok)) return leaf("mn", tok);
    if (tok[0] !== "\\") return /[A-Za-z]/.test(tok) ? leaf("mi", tok) : leaf("mo", tok);
    const name = tok.slice(1);
    switch (name) {
      case "frac":
      case "dfrac":
      case "tfrac":
        return el("mfrac", arg(), arg());
      case "sqrt": {
        if (tokens[at] === "[") {
          at++;
          let index = "";
          while (at < tokens.length && tokens[at] !== "]") index += tokens[at++];
          at++;
          const root = arg();
          return el("mroot", root, leaf("mn", index));
        }
        return el("msqrt", arg());
      }
      case "text":
      case "mathrm":
      case "operatorname":
        return leaf("mtext", text());
      case "left":
      case "right": {
        const d = tokens[at++]?.replace(/^\\/, "") ?? bad();
        return d === "." ? el("mrow") : leaf("mo", d === "{" || d === "lbrace" ? "{" : d === "}" || d === "rbrace" ? "}" : d);
      }
      case "{":
      case "}":
        return leaf("mo", name);
      case "\\":
        return el("mspace");
      default:
    }
    if (SPACES.has(name)) return el("mspace");
    const g = GREEK.get(name);
    if (g) return leaf("mi", g);
    if (FUNCTIONS.has(name)) return leaf("mi", name);
    const s = SYMBOLS.get(name);
    if (s) {
      const op = leaf("mo", s);
      if (LARGE.has(name)) op.setAttribute("largeop", "true");
      return op;
    }
    return bad();
  }

  /** An atom with its ^ and _ . */
  function scripted(): Node {
    const base = atom();
    let sub: Node | null = null;
    let sup: Node | null = null;
    while (tokens[at] === "^" || tokens[at] === "_") {
      const up = tokens[at++] === "^";
      const x = arg();
      if (up) sup = x;
      else sub = x;
    }
    if (sub && sup) return el("msubsup", base, sub, sup);
    if (sup) return el("msup", base, sup);
    if (sub) return el("msub", base, sub);
    return base;
  }

  const kids: Node[] = [];
  while (at < tokens.length) kids.push(scripted());
  const math = el("math", el("mrow", ...kids));
  if (display) math.setAttribute("display", "block");
  return math;
}
