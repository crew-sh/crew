//! Whether two ownership globs can match a common path (`**`, `*`, `?`, `[..]`, `{a,b}`).

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Star,
    Any,
    Lit(char),
}

fn expand_braces(pat: &str) -> Vec<String> {
    let Some(open) = pat.find('{') else {
        return vec![pat.to_string()];
    };
    let mut depth = 0;
    let mut close = None;
    let mut cuts = vec![open];
    for (i, c) in pat[open..].char_indices() {
        let i = open + i;
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            ',' if depth == 1 => cuts.push(i),
            _ => {}
        }
    }
    let Some(close) = close else {
        return vec![pat.to_string()];
    };
    cuts.push(close);
    let (head, tail) = (&pat[..open], &pat[close + 1..]);
    let mut out = Vec::new();
    for w in cuts.windows(2) {
        for rest in expand_braces(&format!("{}{tail}", &pat[w[0] + 1..w[1]])) {
            out.extend(expand_braces(&format!("{head}{rest}")));
        }
    }
    out
}

fn tokens(seg: &str) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    let mut chars = seg.chars();
    while let Some(c) = chars.next() {
        let t = match c {
            '*' => Tok::Star,
            '?' => Tok::Any,
            '[' => {
                for n in chars.by_ref() {
                    if n == ']' {
                        break;
                    }
                }
                Tok::Any
            }
            c => Tok::Lit(c),
        };
        if !(t == Tok::Star && out.last() == Some(&Tok::Star)) {
            out.push(t);
        }
    }
    out
}

fn seg_intersect(p: &[Tok], q: &[Tok]) -> bool {
    fn go(p: &[Tok], q: &[Tok], i: usize, j: usize, memo: &mut Vec<Option<bool>>) -> bool {
        let w = q.len() + 1;
        if let Some(r) = memo[i * w + j] {
            return r;
        }
        let r = match (p.get(i), q.get(j)) {
            (None, None) => true,
            (Some(Tok::Star), _) => {
                go(p, q, i + 1, j, memo) || (j < q.len() && go(p, q, i, j + 1, memo))
            }
            (_, Some(Tok::Star)) => {
                go(p, q, i, j + 1, memo) || (i < p.len() && go(p, q, i + 1, j, memo))
            }
            (Some(a), Some(b)) => {
                (*a == Tok::Any || *b == Tok::Any || a == b) && go(p, q, i + 1, j + 1, memo)
            }
            _ => false,
        };
        memo[i * w + j] = Some(r);
        r
    }
    let mut memo = vec![None; (p.len() + 1) * (q.len() + 1)];
    go(p, q, 0, 0, &mut memo)
}

fn segments(pat: &str) -> Vec<&str> {
    pat.trim_start_matches("./")
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

fn path_intersect(a: &[&str], b: &[&str]) -> bool {
    fn go(a: &[&str], b: &[&str], i: usize, j: usize, memo: &mut Vec<Option<bool>>) -> bool {
        let w = b.len() + 1;
        if let Some(r) = memo[i * w + j] {
            return r;
        }
        let r = match (a.get(i), b.get(j)) {
            (None, None) => true,
            (Some(&"**"), _) => {
                go(a, b, i + 1, j, memo) || (j < b.len() && go(a, b, i, j + 1, memo))
            }
            (_, Some(&"**")) => {
                go(a, b, i, j + 1, memo) || (i < a.len() && go(a, b, i + 1, j, memo))
            }
            (Some(x), Some(y)) => {
                seg_intersect(&tokens(x), &tokens(y)) && go(a, b, i + 1, j + 1, memo)
            }
            _ => false,
        };
        memo[i * w + j] = Some(r);
        r
    }
    let mut memo = vec![None; (a.len() + 1) * (b.len() + 1)];
    go(a, b, 0, 0, &mut memo)
}

/// A pattern owns the path it names and everything under it.
fn overlap_one(a: &str, b: &str) -> bool {
    let (sa, sb) = (segments(a), segments(b));
    let mut da = sa.clone();
    da.push("**");
    let mut db = sb.clone();
    db.push("**");
    path_intersect(&sa, &sb) || path_intersect(&da, &sb) || path_intersect(&sa, &db)
}

pub fn overlaps(a: &str, b: &str) -> bool {
    let bs = expand_braces(b);
    expand_braces(a)
        .iter()
        .any(|x| bs.iter().any(|y| overlap_one(x, y)))
}

#[cfg(test)]
mod tests {
    use super::overlaps;

    #[test]
    fn directories_contain_their_files() {
        assert!(overlaps("crates/a/**", "crates/a/src/x.rs"));
        assert!(overlaps("crates/a", "crates/a/src/x.rs"));
        assert!(overlaps("crates/a/", "crates/a/**"));
        assert!(!overlaps("crates/a/**", "crates/b/**"));
        assert!(!overlaps("crates/a", "crates/ab"));
    }

    #[test]
    fn wildcards_inside_segments() {
        assert!(overlaps("src/*.rs", "src/main.rs"));
        assert!(overlaps("src/ma*", "src/*in.rs"));
        assert!(!overlaps("src/*.rs", "src/*.toml"));
        assert!(!overlaps("src/*.rs", "src/sub/x.rs"));
        assert!(overlaps("src/**/*.rs", "src/sub/x.rs"));
        assert!(overlaps("**/x.rs", "a/b/x.rs"));
        assert!(
            overlaps("**/x.rs", "a/b/y.rs"),
            "a bare path may be a directory"
        );
    }

    #[test]
    fn braces_expand() {
        assert!(overlaps("crates/{a,b}/**", "crates/b/src"));
        assert!(!overlaps("crates/{a,b}/**", "crates/c/src"));
        assert!(overlaps("x/{a,{b,c}}.rs", "x/c.rs"));
    }
}
