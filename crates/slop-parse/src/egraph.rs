//! Bounded egglog saturation over the term language and laws of [`crate::equiv`].
//! The ablation arm: if it proves no more pairs than the normalizer, the e-graph
//! has not earned its dependency weight in the default build.

use anyhow::{anyhow, Result};
use egglog::EGraph;

use crate::equiv::{Term, Tier};

/// Datatypes plus the sound laws. "Exits" and "terminates" are Datalog relations:
/// the control-flow laws are conditional on facts about whole blocks.
const SOUND: &str = r##"
(datatype*
  (Term (Local String) (Name String) (Lit String) (Node String TList) (Block TList))
  (TList (Nil) (Cons Term TList)))
(constructor append (TList TList) TList)
(rewrite (append (Nil) b) b)
(rewrite (append (Cons h t) b) (Cons h (append t b)))
(relation exits (Term))
(rule ((= s (Node "return" r))) ((exits s)))
(rule ((= s (Node "raise" r))) ((exits s)))
(rule ((= s (Node "break" r))) ((exits s)))
(rule ((= s (Node "continue" r))) ((exits s)))
(relation terminates (TList))
(rule ((= l (Cons s (Nil))) (exits s)) ((terminates l)))
(rule ((= l (Cons s rest)) (terminates rest)) ((terminates l)))
(rule ((= l (Cons (Node "if" (Cons c (Cons (Block t) (Cons (Block e) (Nil))))) (Nil))) (terminates t) (terminates e))
      ((terminates l)))
(rule ((= l (Cons s (Cons x rest))) (exits s)) ((union l (Cons s (Nil)))))
(rule ((= l (Cons (Node "if" (Cons c (Cons (Block t) (Cons (Block e) (Nil))))) (Cons r rs))) (terminates t))
      ((union l (Cons (Node "if" (Cons c (Cons (Block t) (Cons (Block (append e (Cons r rs))) (Nil))))) (Nil)))))
(rule ((= l (Cons (Node "if" (Cons c (Cons (Block t) (Cons (Block e) (Nil))))) (Cons r rs))) (terminates e))
      ((union l (Cons (Node "if" (Cons c (Cons (Block (append t (Cons r rs))) (Cons (Block e) (Nil))))) (Nil)))))
(rewrite (Node "if" (Cons (Node "not" (Cons c (Nil))) (Cons a (Cons b (Nil)))))
         (Node "if" (Cons c (Cons b (Cons a (Nil))))))
(rewrite (Node "ifexp" (Cons (Node "not" (Cons c (Nil))) (Cons a (Cons b (Nil)))))
         (Node "ifexp" (Cons c (Cons b (Cons a (Nil))))))
(rewrite (Node "if" (Cons c (Cons (Block (Cons (Node "return" (Cons a (Nil))) (Nil)))
                              (Cons (Block (Cons (Node "return" (Cons b (Nil))) (Nil))) (Nil)))))
         (Node "return" (Cons (Node "ifexp" (Cons c (Cons a (Cons b (Nil))))) (Nil))))
"##;

fn graded_rules() -> String {
    let binary = |op: &str, a: &str, b: &str| format!("(Node \"{op}\" (Cons {a} (Cons {b} (Nil))))");
    let not = |x: &str| format!("(Node \"not\" (Cons {x} (Nil)))");
    let mut rules = vec![
        format!("(rewrite {} {})", binary(">", "a", "b"), binary("<", "b", "a")),
        format!("(rewrite {} {})", binary(">=", "a", "b"), binary("<=", "b", "a")),
        format!("(rewrite {} x)", not(&not("x"))),
        format!("(birewrite {} {})", not(&binary("and", "a", "b")), binary("or", &not("a"), &not("b"))),
        format!("(birewrite {} {})", not(&binary("or", "a", "b")), binary("and", &not("a"), &not("b"))),
    ];
    for op in ["+", "*", "==", "!=", "&", "|", "^"] {
        rules.push(format!("(rewrite {} {})", binary(op, "a", "b"), binary(op, "b", "a")));
    }
    for op in ["+", "*"] {
        rules.push(format!(
            "(birewrite {} {})",
            binary(op, &binary(op, "a", "b"), "c"),
            binary(op, "a", &binary(op, "b", "c"))
        ));
    }
    for op in ["+", "-", "*", "/", "//", "%", "**", "@", "&", "|", "^", "<<", ">>"] {
        rules.push(format!(
            "(rewrite {} (Node \"assign\" (Cons t (Cons {} (Nil)))))",
            binary(&format!("aug:{op}"), "t", "v"),
            binary(op, "t", "v")
        ));
    }
    rules.join("\n")
}

fn write_term(out: &mut String, term: &Term) {
    let list = |out: &mut String, children: &[Term]| {
        for child in children {
            out.push_str("(Cons ");
            write_term(out, child);
            out.push(' ');
        }
        out.push_str("(Nil)");
        out.extend(std::iter::repeat_n(')', children.len()));
    };
    match term {
        Term::Local(name) => out.push_str(&format!("(Local \"{name}\")")),
        Term::Name(name) => out.push_str(&format!("(Name \"{name}\")")),
        Term::Lit(value) => out.push_str(&format!("(Lit \"{value}\")")),
        Term::Node(label, children) => {
            out.push_str(&format!("(Node \"{label}\" "));
            list(out, children);
            out.push(')');
        }
        Term::Block(children) => {
            out.push_str("(Block ");
            list(out, children);
            out.push(')');
        }
    }
}

/// Are `a` and `b` equal under the tier's laws within `iterations` rounds?
/// `false` means "not proven", never "proven different".
pub fn equal(a: &Term, b: &Term, tier: Tier, iterations: usize) -> Result<bool> {
    let mut program = String::from(SOUND);
    if tier == Tier::Graded {
        program.push_str(&graded_rules());
    }
    for (name, term) in [("lhs", a), ("rhs", b)] {
        program.push_str(&format!("\n(let {name} "));
        write_term(&mut program, term);
        program.push(')');
    }
    let mut egraph = EGraph::default();
    egraph
        .parse_and_run_program(None, &format!("{program}\n(run {iterations})"))
        .map_err(|error| anyhow!("egglog saturation failed: {error}"))?;
    Ok(egraph.parse_and_run_program(None, "(check (= lhs rhs))").is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::equiv::equivalence_facts;

    fn term(src: &str) -> Term {
        equivalence_facts(src).unwrap().remove(0).term
    }

    fn proves(a: &str, b: &str, tier: Tier) -> bool {
        equal(&term(a), &term(b), tier, 12).unwrap()
    }

    #[test]
    fn sound_laws_prove_what_the_normalizer_proves() {
        let a = "def f(x, c):\n    if c:\n        return x + 1\n    return x - 1\n";
        let b = "def f(y, flag):\n    return y + 1 if flag else y - 1\n";
        assert!(proves(a, b, Tier::Sound));
        let neg = "def f(x):\n    if not x > 0:\n        z = h(x)\n    else:\n        z = g(x)\n    return z\n";
        let pos = "def f(x):\n    if x > 0:\n        y = g(x)\n    else:\n        y = h(x)\n    return y\n";
        assert!(proves(neg, pos, Tier::Sound));
    }

    #[test]
    fn traps_stay_apart_in_the_sound_tier() {
        let add = "def f(a, b):\n    return a + b\n";
        let swapped = "def f(a, b):\n    return b + a\n";
        assert!(!proves(add, swapped, Tier::Sound));
        assert!(proves(add, swapped, Tier::Graded));
    }
}
