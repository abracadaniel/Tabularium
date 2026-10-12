//! Line-based text diff (Myers' O(ND) algorithm) for side-by-side and unified DDL comparison.

/// One line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// Line present in both texts.
    Same(String),
    /// Line only in the new text.
    Added(String),
    /// Line only in the old text.
    Removed(String),
}

/// Diff two texts line by line. Removed lines are listed before added lines within a change.
pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // common prefix / suffix
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let mut out: Vec<DiffLine> = a[..pre].iter().map(|l| DiffLine::Same(l.to_string())).collect();
    out.extend(myers(&a[pre..a.len() - suf], &b[pre..b.len() - suf]));
    out.extend(a[a.len() - suf..].iter().map(|l| DiffLine::Same(l.to_string())));
    out
}

/// Beyond this many edits the diff degrades to "replace everything" (bounds memory to
/// O(MAX_EDIT_DISTANCE^2)).
const MAX_EDIT_DISTANCE: isize = 4000;

fn myers(a: &[&str], b: &[&str]) -> Vec<DiffLine> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    if n == 0 {
        return b.iter().map(|l| DiffLine::Added(l.to_string())).collect();
    }
    if m == 0 {
        return a.iter().map(|l| DiffLine::Removed(l.to_string())).collect();
    }
    let max = (n + m) as usize;
    let off = max as isize;
    let mut v = vec![0isize; 2 * max + 2];
    // trace[d] holds V (diagonals -d..=d) as it was at the start of round d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=max as isize {
        if d > MAX_EDIT_DISTANCE {
            break;
        }
        trace.push(v[(off - d) as usize..=(off + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let idx = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) { v[idx + 1] } else { v[idx - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let Some(depth) = found else {
        // Too different to diff cheaply: replace everything.
        let mut out: Vec<DiffLine> = a.iter().map(|l| DiffLine::Removed(l.to_string())).collect();
        out.extend(b.iter().map(|l| DiffLine::Added(l.to_string())));
        return out;
    };
    // backtrack
    let mut x = n;
    let mut y = m;
    let mut rev: Vec<DiffLine> = Vec::new();
    for d in (1..=depth).rev() {
        let v = &trace[d as usize];
        let at = |k: isize| v[(k + d) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            rev.push(DiffLine::Same(a[x as usize].to_string()));
        }
        if x == prev_x {
            y -= 1;
            rev.push(DiffLine::Added(b[y as usize].to_string()));
        } else {
            x -= 1;
            rev.push(DiffLine::Removed(a[x as usize].to_string()));
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        rev.push(DiffLine::Same(a[x as usize].to_string()));
    }
    rev.reverse();
    // Within each run of changes, list removals before additions.
    let mut out = Vec::with_capacity(rev.len());
    let mut i = 0;
    while i < rev.len() {
        if matches!(rev[i], DiffLine::Same(_)) {
            out.push(rev[i].clone());
            i += 1;
            continue;
        }
        let start = i;
        while i < rev.len() && !matches!(rev[i], DiffLine::Same(_)) {
            i += 1;
        }
        out.extend(rev[start..i].iter().filter(|l| matches!(l, DiffLine::Removed(_))).cloned());
        out.extend(rev[start..i].iter().filter(|l| matches!(l, DiffLine::Added(_))).cloned());
    }
    out
}

/// Unified diff (`---`/`+++` headers, `@@` hunks with `context` lines). Empty when equal.
pub fn unified_diff(old: &str, new: &str, old_name: &str, new_name: &str, context: usize) -> String {
    let lines = diff_lines(old, new);
    if lines.iter().all(|l| matches!(l, DiffLine::Same(_))) {
        return String::new();
    }
    // old/new line numbers (1-based) before each diff line
    let mut pos = Vec::with_capacity(lines.len());
    let (mut o, mut n) = (1usize, 1usize);
    for l in &lines {
        pos.push((o, n));
        match l {
            DiffLine::Same(_) => {
                o += 1;
                n += 1;
            }
            DiffLine::Removed(_) => o += 1,
            DiffLine::Added(_) => n += 1,
        }
    }
    let changed: Vec<usize> = (0..lines.len()).filter(|&i| !matches!(lines[i], DiffLine::Same(_))).collect();
    // group changes into hunks
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for &c in &changed {
        let start = c.saturating_sub(context);
        let end = (c + context + 1).min(lines.len());
        match hunks.last_mut() {
            Some(h) if start <= h.1 => h.1 = h.1.max(end),
            _ => hunks.push((start, end)),
        }
    }
    let mut out = format!("--- {old_name}\n+++ {new_name}\n");
    for (s, e) in hunks {
        let old_count = lines[s..e].iter().filter(|l| !matches!(l, DiffLine::Added(_))).count();
        let new_count = lines[s..e].iter().filter(|l| !matches!(l, DiffLine::Removed(_))).count();
        let (os, ns) = pos[s];
        let os = if old_count == 0 { os - 1 } else { os };
        let ns = if new_count == 0 { ns - 1 } else { ns };
        out.push_str(&format!("@@ -{os},{old_count} +{ns},{new_count} @@\n"));
        for l in &lines[s..e] {
            match l {
                DiffLine::Same(t) => out.push_str(&format!(" {t}\n")),
                DiffLine::Removed(t) => out.push_str(&format!("-{t}\n")),
                DiffLine::Added(t) => out.push_str(&format!("+{t}\n")),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiffLine::*;

    fn apply(lines: &[DiffLine]) -> (String, String) {
        let mut a = Vec::new();
        let mut b = Vec::new();
        for l in lines {
            match l {
                Same(t) => {
                    a.push(t.clone());
                    b.push(t.clone());
                }
                Removed(t) => a.push(t.clone()),
                Added(t) => b.push(t.clone()),
            }
        }
        (a.join("\n"), b.join("\n"))
    }

    #[test]
    fn basic() {
        let d = diff_lines("a\nb\nc", "a\nx\nc");
        assert_eq!(d, vec![Same("a".into()), Removed("b".into()), Added("x".into()), Same("c".into())]);
        assert_eq!(diff_lines("", "a"), vec![Added("a".into())]);
        assert_eq!(diff_lines("a", ""), vec![Removed("a".into())]);
        assert!(diff_lines("a\nb", "a\nb").iter().all(|l| matches!(l, Same(_))));
    }

    #[test]
    fn reconstructs_inputs() {
        let cases = [
            ("a\nb\nc\nd\ne\nf", "x\nb\nc\ny\ne\nz\nw"),
            ("1\n2\n3\n4\n5\n6\n7\n8", "8\n7\n6\n5\n4\n3\n2\n1"),
            ("abc\nabc\nabc", "abc\nxyz\nabc\nabc\nq"),
            ("", ""),
        ];
        for (old, new) in cases {
            let d = diff_lines(old, new);
            assert_eq!(apply(&d), (old.to_string(), new.to_string()));
        }
        // minimality on a classic example: ABCABBA -> CBABAC has edit distance 5
        let a = "A\nB\nC\nA\nB\nB\nA";
        let b = "C\nB\nA\nB\nA\nC";
        let edits = diff_lines(a, b).iter().filter(|l| !matches!(l, Same(_))).count();
        assert_eq!(edits, 5);
    }

    #[test]
    fn unified() {
        let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9";
        let new = "l1\nl2\nl3\nL4\nl5\nl6\nl7\nl8\nl9\nl10";
        let u = unified_diff(old, new, "a.sql", "b.sql", 1);
        assert_eq!(
            u,
            "--- a.sql\n+++ b.sql\n@@ -3,3 +3,3 @@\n l3\n-l4\n+L4\n l5\n@@ -9,1 +9,2 @@\n l9\n+l10\n"
        );
        assert_eq!(unified_diff("x", "x", "a", "b", 3), "");
        let u = unified_diff("", "new", "a", "b", 3);
        assert_eq!(u, "--- a\n+++ b\n@@ -0,0 +1,1 @@\n+new\n");
    }
}
