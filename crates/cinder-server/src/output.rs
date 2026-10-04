//! Turning compiler output into what the playground displays.
//!
//! The assembly carries `.loc 1 <line>` directives and the annotated IR carries
//! `; L<line>` comments. Both are removed from the text and returned as a
//! `lineMap`: for every displayed line, the 1-based source line it came from (0 = none).

/// Strip `.file`/`.loc` directives from assembly; map each remaining line to its source line.
pub fn clean_asm(raw: &str) -> (String, Vec<u32>) {
    let mut out = String::with_capacity(raw.len());
    let mut map = Vec::new();
    let mut current = 0u32;
    for line in raw.lines() {
        let t = line.trim_start();
        if t.starts_with(".file") {
            continue;
        }
        if let Some(rest) = t.strip_prefix(".loc") {
            if let Some(n) = rest.split_whitespace().nth(1).and_then(|x| x.parse().ok()) {
                current = n;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        map.push(current);
        if t.starts_with(".size") {
            current = 0;
        }
    }
    (out, map)
}

/// Strip the `; L<n>` annotations of `--emit-ir-lines`; map each line to its source line.
pub fn clean_ir(raw: &str) -> (String, Vec<u32>) {
    let mut out = String::with_capacity(raw.len());
    let mut map = Vec::new();
    for line in raw.lines() {
        let mut n = 0;
        let mut text = line;
        if let Some(idx) = line.rfind("  ; L") {
            if let Ok(v) = line[idx + 5..].trim().parse::<u32>() {
                n = v;
                text = &line[..idx];
            }
        }
        out.push_str(text);
        out.push('\n');
        map.push(n);
    }
    (out, map)
}

/// Text that has no source mapping (AST, HIR, preprocessed source).
pub fn plain(raw: &str) -> (String, Vec<u32>) {
    let mut out = String::with_capacity(raw.len());
    let mut map = Vec::new();
    for line in raw.lines() {
        out.push_str(line);
        out.push('\n');
        map.push(0);
    }
    (out, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asm_directives_become_a_line_map() {
        let raw = "\t.file \"main.c\"\n\t.file 1 \"main.c\"\n\t.text\nmain:\n\tpushq %rbp\n\t.loc 1 3\n\tmovl $1, %eax\n\t.loc 1 4\n\tret\n\t.size main, .-main\n\n\t.section .rodata\n";
        let (text, map) = clean_asm(raw);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), map.len());
        assert!(!text.contains(".loc") && !text.contains(".file"));
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
        assert_eq!(map[at("pushq")], 0);
        assert_eq!(map[at("movl")], 3);
        assert_eq!(map[at("ret")], 4);
        assert_eq!(map[at(".size")], 4);
        assert_eq!(map[at(".rodata")], 0, "the mapping stops at the end of a function");
    }

    #[test]
    fn ir_annotations_become_a_line_map() {
        let raw = "define i32 @f(i32 %a.0) {\nentry:\n  %1 = add i32 %a.0, 1  ; L2\n  ret i32 %1  ; L3\n}\n";
        let (text, map) = clean_ir(raw);
        assert_eq!(text, "define i32 @f(i32 %a.0) {\nentry:\n  %1 = add i32 %a.0, 1\n  ret i32 %1\n}\n");
        assert_eq!(map, vec![0, 0, 2, 3, 0]);
    }

    #[test]
    fn plain_text_has_an_all_zero_map() {
        let (t, m) = plain("a\nb\n");
        assert_eq!((t.as_str(), m), ("a\nb\n", vec![0, 0]));
    }
}
