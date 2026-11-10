//! Where each labelled part's placement is written in the script.
//!
//! A part can be moved when its map literal has a string `label` and an `at` that is either an
//! `[x, y, z]` literal of plain numbers or a variable set once to one (`let lamp_at = [...]`),
//! and that literal is what the part actually got. Parts that share a variable share a handle,
//! so they move together. `rotate` is found the same way; a movable part without one can still
//! be turned: the preview inserts `, rotate: [...]` right after its `at`. Everything else
//! (computed positions, parts made in a loop) still previews, but can't be moved.

use std::collections::HashMap;

use rhai::{ASTNode, Expr, Position, Stmt};
use serde::Serialize;

/// The text of one `[x, y, z]` literal, as UTF-16 offsets (JavaScript string indices).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Handle {
    pub start: usize,
    pub end: usize,
    pub value: [f64; 3],
}

/// Where one part's placement is written: indexes into the handle list.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Placement {
    /// The text of its `at`, if it can be moved.
    pub at: Option<usize>,
    /// The text of its `rotate`, if it has one that can be rewritten.
    pub rotate: Option<usize>,
    /// When it can be moved but has no `rotate`: where to insert `, rotate: [...]` (UTF-16).
    pub insert: Option<usize>,
}

/// What a part's map literal says about its placement.
#[derive(Default)]
struct Written {
    at: Option<Source>,
    /// Byte offset just past the `at` value in the map.
    at_end: Option<usize>,
    rotate: Option<Source>,
    has_rotate: bool,
}

enum Source {
    Literal(Position),
    Variable(String),
}

/// The handles in `script`, and for each part (by its label and its evaluated `at` and
/// `rotate`), where its placement is written.
pub fn find_handles(script: &str, labels: &[Option<&str>], ats: &[[f64; 3]], rotates: &[[f64; 3]]) -> (Vec<Handle>, Vec<Placement>) {
    let none = || (vec![], vec![Placement::default(); labels.len()]);
    let mut engine = fidget::rhai::engine();
    // Keep the syntax tree as written: optimizing would fold literals into constants.
    engine.set_optimization_level(rhai::OptimizationLevel::None);
    let Ok(ast) = engine.compile(script) else { return none() };

    // `let name = [...]` literals (a name bound more than once is ambiguous: None).
    let mut lets: HashMap<String, Option<Position>> = HashMap::new();
    // label -> what its map literal says (None if the label is written more than once).
    let mut written: HashMap<String, Option<Written>> = HashMap::new();
    ast.walk(&mut |path: &[ASTNode]| {
        match path.last() {
            Some(ASTNode::Stmt(Stmt::Var(x, ..))) => {
                let (ident, init, _) = &**x;
                let pos = match init {
                    Expr::Array(_, pos) => Some(*pos),
                    _ => None,
                };
                lets.entry(ident.name.to_string()).and_modify(|e| *e = None).or_insert(pos);
            }
            Some(ASTNode::Expr(Expr::Map(m, _))) => {
                let field = |name: &str| m.0.iter().find(|(k, _)| k.name == name).map(|(_, v)| v);
                if let Some(Expr::StringConstant(label, _)) = field("label") {
                    let source = |e: &Expr| match e {
                        Expr::Array(_, pos) => Some(Source::Literal(*pos)),
                        Expr::Variable(v, _, _) => Some(Source::Variable(v.1.to_string())),
                        _ => None,
                    };
                    let at_end = field("at").and_then(|e| match e {
                        Expr::Array(_, pos) => literal_at(script, *pos).map(|(_, end, _)| end),
                        Expr::Variable(v, _, pos) => offset(script, *pos).map(|start| start + v.1.len()),
                        _ => None,
                    });
                    let w = Written {
                        at: field("at").and_then(source),
                        at_end,
                        rotate: field("rotate").and_then(source),
                        has_rotate: field("rotate").is_some(),
                    };
                    written.entry(label.to_string()).and_modify(|e| *e = None).or_insert(Some(w));
                }
            }
            _ => {}
        }
        true
    });

    let mut count: HashMap<&str, usize> = HashMap::new();
    for label in labels.iter().flatten() {
        *count.entry(label).or_default() += 1;
    }
    let mut handles: Vec<Handle> = vec![];
    let mut intern = |h: Handle| match handles.iter().position(|x| *x == h) {
        Some(i) => i,
        None => {
            handles.push(h);
            handles.len() - 1
        }
    };
    // The literal a source names, if it holds `value`.
    let resolve = |source: &Source, value: &[f64; 3]| -> Option<Handle> {
        let pos = match source {
            Source::Literal(pos) => *pos,
            Source::Variable(name) => (*lets.get(name)?)?,
        };
        let (start, end, v) = literal_at(script, pos)?;
        if v.iter().zip(value).any(|(a, b)| (a - b).abs() > 1e-6) {
            return None; // the literal isn't what the part ended up with
        }
        Some(Handle { start: utf16(script, start), end: utf16(script, end), value: v })
    };
    let placements = (0..labels.len())
        .map(|i| {
            let Some(label) = labels[i] else { return Placement::default() };
            if count[label] != 1 {
                return Placement::default(); // e.g. made in a loop
            }
            let Some(Some(w)) = written.get(label) else { return Placement::default() };
            let Some(at) = w.at.as_ref().and_then(|s| resolve(s, &ats[i])) else { return Placement::default() };
            let rotate = w.rotate.as_ref().and_then(|s| resolve(s, &rotates[i]));
            let insert = (!w.has_rotate).then_some(w.at_end).flatten().map(|end| utf16(script, end));
            Placement { at: Some(intern(at)), rotate: rotate.map(&mut intern), insert }
        })
        .collect();
    (handles, placements)
}

/// The byte offset of a Rhai position (1-based line and column, in characters).
fn offset(script: &str, pos: Position) -> Option<usize> {
    let (line, col) = (pos.line()?, pos.position()?);
    let line_start: usize = script.split_inclusive('\n').take(line - 1).map(str::len).sum();
    Some(line_start + script[line_start..].char_indices().nth(col - 1)?.0)
}

fn utf16(script: &str, byte: usize) -> usize {
    script[..byte].encode_utf16().count()
}

/// The `[x, y, z]` literal starting at `pos`, if it is three plain numbers: its byte range and value.
fn literal_at(script: &str, pos: Position) -> Option<(usize, usize, [f64; 3])> {
    let start = offset(script, pos)?;
    if !script[start..].starts_with('[') {
        return None;
    }
    let end = start + script[start..].find(']')? + 1;
    let nums: Vec<f64> = script[start + 1..end - 1]
        .split(',')
        .map(|s| s.trim().replace('_', "").parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()?;
    Some((start, end, nums.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_literals_and_shared_variables() {
        let script = "let lamp_at = [1.5, -2, 0];\n\
            #{ parts: [\n\
              #{ label: \"base\", shape: sphere([0, 0, 0], 1), at: [0, 0, 1], rotate: [0, 0, 45] },\n\
              #{ label: \"glass\", shape: sphere(), at: lamp_at },\n\
              #{ label: \"glow\", shape: sphere(), at: lamp_at },\n\
              #{ label: \"computed\", shape: sphere(), at: [1 + 1, 0, 0] },\n\
            ] }";
        let labels = [Some("base"), Some("glass"), Some("glow"), Some("computed")];
        let ats = [[0.0, 0.0, 1.0], [1.5, -2.0, 0.0], [1.5, -2.0, 0.0], [2.0, 0.0, 0.0]];
        let rotates = [[0.0, 0.0, 45.0], [0.0; 3], [0.0; 3], [0.0; 3]];
        let (handles, placed) = find_handles(script, &labels, &ats, &rotates);
        let text = |i: usize| &script[handles[i].start..handles[i].end];
        assert_eq!(text(placed[0].at.unwrap()), "[0, 0, 1]");
        assert_eq!(text(placed[0].rotate.unwrap()), "[0, 0, 45]");
        assert_eq!(placed[0].insert, None, "it has a rotate already");
        assert_eq!(placed[1].at, placed[2].at, "glass and glow share lamp_at");
        assert_eq!(text(placed[1].at.unwrap()), "[1.5, -2, 0]");
        // Without a rotate, one goes right after `at: lamp_at`.
        let insert = placed[1].insert.unwrap();
        assert!(script[..insert].ends_with("#{ label: \"glass\", shape: sphere(), at: lamp_at"));
        assert_eq!(placed[3], Placement::default());
    }

    #[test]
    fn offsets_are_utf16() {
        let script = "// é 🙂\n[#{ label: \"a\", shape: sphere(), at: [1, 2, 3] }]";
        let (handles, placed) = find_handles(script, &[Some("a")], &[[1.0, 2.0, 3.0]], &[[0.0; 3]]);
        let utf16: Vec<u16> = script.encode_utf16().collect();
        let h = &handles[placed[0].at.unwrap()];
        assert_eq!(String::from_utf16(&utf16[h.start..h.end]).unwrap(), "[1, 2, 3]");
        let insert = placed[0].insert.unwrap();
        assert_eq!(String::from_utf16(&utf16[..insert]).unwrap().chars().last(), Some(']'));
    }
}
