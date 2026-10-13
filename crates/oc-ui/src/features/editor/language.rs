//! The language shown in the status line, picked the way 1.x picked its
//! Pygments lexer (`_lexer_for`): by file name, with the file's first 4096
//! characters breaking ties, else for "#!" scripts by the interpreter. The
//! name is Pygments' ("Bash", "JavaScript", "Transact-SQL"); highlighting
//! uses the syntect syntax for that language, if there is one.

use std::sync::OnceLock;

use regex::Regex;

use super::language_table::PATTERNS;

/// How much of the text Pygments looked at (1.x: `text[:4096]`).
const SNIFF_CHARS: usize = 4096;

/// The status line's language and the syntect syntax to highlight with.
/// `("Plain text", None)` for files Pygments had no lexer for.
pub fn language_for(file_name: &str, text: &str) -> (String, Option<&'static str>) {
    let head = match text.char_indices().nth(SNIFF_CHARS) {
        Some((i, _)) => &text[..i],
        None => text,
    };
    let name = by_file_name(file_name, head).or_else(|| if head.starts_with("#!") { guess(head) } else { None });
    match name {
        Some(n) if !n.is_empty() => (n.to_owned(), syntax_name(n)),
        _ => ("Plain text".to_owned(), None),
    }
}

/// Pygments' `get_lexer_for_filename`: of the lexers with a matching
/// pattern, the best rated: an exact name (no `*`) gets half a point, then
/// how sure the lexer is about the text (its priority for an empty text),
/// then the class name.
fn by_file_name(file_name: &str, text: &str) -> Option<&'static str> {
    let mut best: Option<(f32, &str, &str)> = None;
    for &(pattern, name, class, priority) in PATTERNS {
        if !glob(pattern, file_name) {
            continue;
        }
        let bonus = if pattern.contains('*') { 0.0 } else { 0.5 };
        let rating = if text.is_empty() { priority } else { analyse(class, text) } + bonus;
        if best.is_none_or(|(r, c, _)| (rating, class) > (r, c)) {
            best = Some((rating, class, name));
        }
    }
    best.map(|(_, _, name)| name)
}

/// `fnmatch` without case folding (`*`, `?`, `[...]`), as Pygments matches.
fn glob(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let Some(end) = p.iter().skip(2).position(|c| *c == ']').map(|i| i + 2) else {
                    return n.first() == Some(&'[') && go(&p[1..], &n[1..]);
                };
                let Some(&c) = n.first() else { return false };
                let set = &p[1..end];
                let hit = set.windows(3).any(|w| w[1] == '-' && w[0] <= c && c <= w[2]) || set.contains(&c);
                hit && go(&p[end + 1..], &n[1..])
            }
            Some(&c) => n.first() == Some(&c) && go(&p[1..], &n[1..]),
        }
    }
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    go(&p, &n)
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

macro_rules! regex {
    ($p:expr) => {{
        static CELL: OnceLock<Regex> = OnceLock::new();
        re(&CELL, $p)
    }};
}

/// Pygments' `shebang_matches`: the interpreter on the "#!" line (its last
/// word that isn't an option, e.g. "python3" in "/usr/bin/env python3")
/// matches `interpreter`, a regex put between `^` and
/// `(\.(exe|cmd|bat|bin))?$` as Pygments does (so "a|b" is unanchored).
fn shebang_matches(text: &str, interpreter: &str) -> bool {
    let first = text.split('\n').next().unwrap_or("").to_lowercase();
    let Some(rest) = first.strip_prefix("#!") else {
        return false;
    };
    let Some(found) = rest.trim().split(['/', '\\', ' ']).rfind(|x| !x.is_empty() && !x.starts_with('-')) else {
        return false;
    };
    Regex::new(&format!(r"(?i)^{interpreter}(\.(exe|cmd|bat|bin))?$")).is_ok_and(|re| re.is_match(found))
}

/// The lexers' `analyse_text` for the ambiguous patterns people meet (".h",
/// ".m", ".pl", ".sql", ".s", …), clamped to 0..1 like Pygments. Others
/// rate 0, so Pygments' tie-break (the class name) decides, as it did.
fn analyse(class: &str, text: &str) -> f32 {
    let objc = || {
        if regex!(r"@(?:end|implementation|protocol)").is_match(text) {
            1.0
        } else if text.contains("@\"") {
            0.8
        } else if regex!(r"@[0-9]+").is_match(text) {
            0.7
        } else if regex!(r"\[\s*[a-zA-Z_]\w*\s+(?:[a-zA-Z_]\w*\s*\]|(?:[a-zA-Z_]\w*)?:)").is_match(text) {
            0.8
        } else {
            0.0
        }
    };
    let rating: f32 = match class {
        "CLexer" => {
            if regex!(r#"(?m)^\s*#include [<"]"#).is_match(text) || regex!(r"(?m)^\s*#ifn?def ").is_match(text) {
                0.1
            } else {
                0.0
            }
        }
        "CppLexer" => {
            if regex!(r"#include <[a-z_]+>").is_match(text) {
                0.2
            } else if text.contains("using namespace ") {
                0.4
            } else {
                0.0
            }
        }
        "ObjectiveCLexer" | "ObjectiveCppLexer" => objc(),
        "PerlLexer" => {
            if shebang_matches(text, "perl") {
                1.0
            } else {
                let r = if regex!(r"(?:my|our)\s+[$@%(]").is_match(text) { 0.9 } else { 0.0 };
                if text.contains(":=") { r / 2.0 } else { r }
            }
        }
        "Perl6Lexer" => perl6(text),
        "PrologLexer" => {
            if text.contains(":-") {
                0.5
            } else {
                0.0
            }
        }
        "MatlabLexer" => {
            // As in Pygments, which tests the whole text, not each line.
            let first = if regex!(r"^\s*%").is_match(text) { "" } else { text.lines().next().unwrap_or("").trim() };
            if first.starts_with("function") && !first.contains('{') {
                1.0
            } else if regex!(r"(?m)^\s*%").is_match(text) || regex!(r"(?m)^!\w+").is_match(text) {
                0.2
            } else {
                0.0
            }
        }
        "TransactSqlLexer" => {
            if regex!(r"(?i)\bdeclare\s+@").is_match(text) {
                1.0
            } else {
                let backticks = regex!(r"`[a-zA-Z_]\w*`").find_iter(text).count();
                let brackets = regex!(r"\[[a-zA-Z_]\w*\]").find_iter(text).count();
                let mut r = if backticks + brackets >= 1 && brackets >= 2 * backticks {
                    0.5
                } else if brackets > backticks {
                    0.2
                } else if brackets > 0 {
                    0.1
                } else {
                    0.0
                };
                if regex!(r"@[a-zA-Z_]\w*\b").is_match(text) {
                    r += 0.1;
                }
                if regex!(r"(?i)\bgo\b").is_match(text) {
                    r += 0.1;
                }
                r
            }
        }
        "TasmLexer" => {
            if regex!(r"(?i)^PROC").is_match(text) {
                1.0
            } else {
                0.0
            }
        }
        "GasLexer" => {
            if regex!(r"(?m)^\.(text|data|section)").is_match(text) {
                1.0
            } else if regex!(r"(?m)^\.\w+").is_match(text) {
                0.1
            } else {
                0.0
            }
        }
        "ForthLexer" => {
            if regex!(r"\n:[^\n]+;\n").is_match(text) {
                0.3
            } else {
                0.0
            }
        }
        "FSharpLexer" => 0.05 * (text.contains("|>") as u8 + text.contains("<|") as u8) as f32,
        "VerilogLexer" => 0.1 * ["reg", "wire", "assign"].iter().filter(|w| text.contains(*w)).count() as f32,
        "RocqLexer" => {
            if text.contains("Qed") && text.contains("Proof") {
                1.0
            } else {
                0.0
            }
        }
        "XsltLexer" => {
            if looks_like_xml(text) && text.contains("<xsl") {
                0.8
            } else {
                0.0
            }
        }
        "MasonLexer" => {
            if regex!(r"</%(class|doc|init)>").is_match(text) {
                1.0
            } else if regex!(r"(?s)<&.+&>").is_match(text) {
                0.11
            } else {
                0.0
            }
        }
        "PhpLexer" => {
            if shebang_matches(text, "php") {
                1.0
            } else if text.match_indices("<?").any(|(i, _)| !text[i + 2..].starts_with("xml")) {
                0.3
            } else {
                0.0
            }
        }
        "PythonLexer" => python(text),
        "XmlLexer" => {
            if looks_like_xml(text) {
                0.45
            } else {
                0.0
            }
        }
        "HtmlLexer" => {
            let html = regex!(r"(?i)^html").is_match(doctype(text).unwrap_or("").trim());
            if html { 0.5 } else { 0.0 }
        }
        "SqlJinjaLexer" => {
            // dbt's ref() and source(), Jinja macros.
            let mut r = 0.0;
            if regex!(r"\{\{\s*ref\(.*\)\s*\}\}").is_match(text) {
                r += 0.4;
            }
            if regex!(r"\{\{\s*source\(.*\)\s*\}\}").is_match(text) {
                r += 0.25;
            }
            if regex!(r"\{%-?\s*macro \w+\(.*\)\s*-?%\}").is_match(text) {
                r += 0.15;
            }
            r
        }
        "SwigLexer" => {
            let mut r = 0.0;
            for c in regex!(r"(?m)^\s*(%[a-z_][a-z0-9_]*)").captures_iter(text) {
                if SWIG_DIRECTIVES.contains(&&c[1][1..]) {
                    r = 0.98;
                    break;
                }
                r = 0.91;
            }
            r
        }
        // Only at the very start: Pygments searches without MULTILINE.
        "VisualPrologLexer" => {
            let re = regex!(r"^\s*(end\s+(interface|class|implement)|(clauses|predicates|domains|facts|constants|properties)\s*$)");
            if re.is_match(text) { 0.98 } else { 0.0 }
        }
        "JasminLexer" => {
            let mut r = 0.0;
            if regex!(r"(?m)^\s*\.class\s").is_match(text) {
                r += 0.5;
                if regex!(r"(?m)^\s*[a-z]+_[a-z]+\b").is_match(text) {
                    r += 0.3;
                }
            }
            let directive = r"(?m)^\s*\.(attribute|bytecode|debug|deprecated|enclosing|inner|interface|limit|set|signature|stack)\b";
            if regex!(directive).is_match(text) {
                r += 0.6;
            }
            r
        }
        "ObjectiveJLexer" => {
            if regex!(r#"(?m)^\s*@import\s+[<"]"#).is_match(text) { 1.0 } else { 0.0 }
        }
        "Lean3Lexer" => {
            if regex!(r"(?m)^import [a-z]").is_match(text) { 0.1 } else { 0.0 }
        }
        "Lean4Lexer" => {
            if regex!(r"(?m)^import [A-Z]").is_match(text) { 0.1 } else { 0.0 }
        }
        "MapleLexer" => {
            if text.contains(":=") { 0.1 } else { 0.0 }
        }
        "SuperColliderLexer" => {
            if text.contains("SinOsc") || text.contains("thisFunctionDef") { 0.1 } else { 0.0 }
        }
        "ScdocLexer" => {
            let mut r = 0.01 * (text.contains('*') as u8 + text.contains('_') as u8) as f32;
            if regex!(r#"^.*\([1-7]\)( "[^"]+"){0,2}$"#).is_match(text.split('\n').next().unwrap_or("")) {
                r += 0.5;
            }
            r
        }
        "Modula2Lexer" => {
            if !(text.contains("(*") && text.contains("*)") && text.contains(":=")) || regex!(r"\bFUNCTION\b").is_match(text) {
                0.0
            } else if regex!(r"\bPROCEDURE\b").is_match(text) {
                0.6
            } else {
                0.0
            }
        }
        "SingularityLexer" => {
            let mut r = 0.0;
            if regex!(r"(?i)\b(?:osversion|includecmd|mirrorurl)\b").is_match(text) {
                r += 0.5;
            }
            if regex!(r"(%(?:pre|post|setup|environment|help|labels|test|runscript|files|startscript))(\s*)").is_match(text) {
                r += 0.49;
            }
            r
        }
        _ => 0.0,
    };
    rating.clamp(0.0, 1.0)
}

fn python(text: &str) -> f32 {
    let head: String = text.chars().take(1000).collect();
    if shebang_matches(text, r"pythonw?(3(\.\d)?)?") || head.contains("import ") { 1.0 } else { 0.0 }
}

/// SWIG's directives (Pygments' `SwigLexer.swig_directives`, without the "%").
const SWIG_DIRECTIVES: &[&str] = &[
    "apply", "define", "director", "enddef", "exception", "extend", "feature", "fragment", "ignore", "immutable", "import",
    "include", "inline", "insert", "module", "newobject", "nspace", "pragma", "rename", "shared_ptr", "template", "typecheck",
    "typemap", "arg", "attribute", "bang", "begin", "callback", "catches", "clear", "constant", "copyctor", "csconst",
    "csconstvalue", "csenum", "csmethodmodifiers", "csnothrowexception", "default", "defaultctor", "defaultdtor", "defined",
    "delete", "delobject", "descriptor", "exceptionclass", "exceptionvar", "extend_smart_pointer", "fragments", "header",
    "ifcplusplus", "ignorewarn", "implicit", "implicitconv", "init", "javaconst", "javaconstvalue", "javaenum",
    "javaexception", "javamethodmodifiers", "kwargs", "luacode", "mutable", "naturalvar", "nestedworkaround", "perlcode",
    "pythonabc", "pythonappend", "pythoncallback", "pythoncode", "pythondynamic", "pythonmaybecall", "pythonnondynamic",
    "pythonprepend", "refobject", "shadow", "sizeof", "trackobjects", "types", "unrefobject", "varargs", "warn", "warnfilter",
];

/// The first part of the text's `<!DOCTYPE …>` (Pygments' `doctype_lookup_re`).
fn doctype(text: &str) -> Option<&str> {
    let re = regex!(r#"(?s)<!DOCTYPE\s+([a-zA-Z_][a-zA-Z0-9]*(?:\s+[a-zA-Z_][a-zA-Z0-9]*\s+"[^"]*")?)[^>]*>"#);
    re.captures(text).map(|c| c.get(1).map_or("", |m| m.as_str()))
}

/// Pygments' `looks_like_xml`: an XML declaration first, a doctype, or in
/// the first 1000 characters a tag with its closing tag.
fn looks_like_xml(text: &str) -> bool {
    if regex!(r"(?i)^\s*<\?xml[^>]*\?>").is_match(text) || doctype(text).is_some() {
        return true;
    }
    let head = match text.char_indices().nth(1000) {
        Some((i, _)) => &text[..i],
        None => text,
    };
    // `<(name)(\s[^>]*)?>.*?</\1\s*>` without the back reference.
    let open = regex!(r"(?i)\A<([a-zA-Z][a-zA-Z0-9._:-]*)(\s[^>]*)?>");
    let close = regex!(r"(?i)</([a-zA-Z][a-zA-Z0-9._:-]*)\s*>");
    head.match_indices('<').any(|(i, _)| {
        open.captures(&head[i..]).is_some_and(|c| {
            let rest = &head[i + c[0].len()..];
            close.captures_iter(rest).any(|e| e[1].eq_ignore_ascii_case(&c[1]))
        })
    })
}

fn perl6(text: &str) -> f32 {
    if shebang_matches(text, r"perl6|rakudo|niecza|pugs") {
        return 1.0;
    }
    // Without POD blocks.
    let mut in_pod = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if regex!(r"^=(?:end|cut)").is_match(line) {
            in_pod = false;
        } else if regex!(r"^=\w+").is_match(line) {
            in_pod = true;
        } else if !in_pod {
            lines.push(line);
        }
    }
    let joined = lines.join("\n");
    let mut saw_decl = false;
    let mut rating = 0.0;
    if regex!(r"(?:my|our|has)\s+(?:[\w':-]+\s+)?[$@%&(]").is_match(&joined) {
        rating = 0.8;
        saw_decl = true;
    }
    for line in text.lines() {
        let line = regex!("#.*").replace(line, "");
        if line.trim().is_empty() {
            continue;
        }
        if regex!(r"^\s*(?:use\s+)?v6(?:\.\d(?:\.\d)?)?;").is_match(&line) {
            return 1.0;
        }
        if let Some(c) = regex!(r"^\s*(?:(my|our)\s+)?(?:module|class|role|enum|grammar)").captures(&line) {
            if saw_decl || c.get(1).is_some() {
                return 1.0;
            }
            rating = 0.05;
            continue;
        }
        break;
    }
    if joined.contains(":=") { rating / 2.0 } else { rating }
}

/// Pygments' `guess_lexer` for "#!" scripts without a known name: the
/// lexers that recognise their interpreter (and Python's "import ").
fn guess(text: &str) -> Option<&'static str> {
    let numpy = || {
        let head: String = text.chars().take(1000).collect();
        head.contains("import numpy") || head.contains("from numpy import")
    };
    const SHEBANGS: [(&str, &str); 11] = [
        (r"(ba|z|)sh", "Bash"),
        (r"execlineb", "execline"),
        (r"groovy", "Groovy"),
        (r"julia", "Julia"),
        (r"mojo?", "Mojo"),
        (r"perl", "Perl"),
        (r"perl6|rakudo|niecza|pugs", "Perl6"),
        (r"php", "JavaScript+PHP"),
        (r"pythonw?2(\.\d)?", "Python 2.x"),
        (r"ruby(1\.\d)?", "Ruby"),
        (r"(tcl)", "Tcl"),
    ];
    // Pygments tries the lexers by class name and takes the first sure one:
    // Python's ("import ") comes after these and before Ruby's and Tcl's.
    let (before, after) = SHEBANGS.split_at(9);
    if let Some((_, name)) = before.iter().find(|(re, _)| shebang_matches(text, re)) {
        return Some(name);
    }
    if python(text) > 0.0 {
        return Some(if numpy() { "NumPy" } else { "Python" });
    }
    after.iter().find(|(re, _)| shebang_matches(text, re)).map(|(_, name)| *name)
}

/// The syntect syntax for a Pygments language: the same name, or the one
/// below. None: shown without colors (rather than in a wrong language).
fn syntax_name(pygments: &'static str) -> Option<&'static str> {
    const SAME_LANGUAGE: &[(&str, &str)] = &[
        ("ActionScript 3", "ActionScript"),
        ("ApacheConf", "Apache Conf"),
        ("Bash", "Bourne Again Shell (bash)"),
        ("Batchfile", "Batch File"),
        ("ClojureScript", "Clojure"),
        ("Coldfusion CFC", "CFML"),
        ("Coldfusion HTML", "CFML"),
        ("Common Lisp", "Lisp"),
        ("CSS+Django/Jinja", "Jinja2"),
        ("Cython", "Python"),
        ("Debian Sourcelist", "debsources"),
        ("Delphi", "Pascal"),
        ("Desktop file", "INI"),
        ("Docker", "Dockerfile"),
        ("E-mail", "Email"),
        ("EmacsLisp", "Lisp"),
        ("Fortran", "Fortran (Modern)"),
        ("FortranFixed", "Fortran (Fixed Form)"),
        ("GDScript", "GDScript (Godot Engine)"),
        ("GoogleSQL", "SQL"),
        ("Graphviz", "Graphviz (DOT)"),
        ("Groff", "Groff/troff"),
        ("HTML+Django/Jinja", "Jinja2"),
        ("HTML+PHP", "PHP"),
        ("HTML+Twig", "HTML (Twig)"),
        ("Haml", "Ruby Haml"),
        ("Hspec", "Haskell"),
        ("Java Server Page", "Java Server Page (JSP)"),
        ("JavaScript", "JavaScript (Babel)"),
        ("JavaScript+Django/Jinja", "Jinja2"),
        ("JavaScript+PHP", "PHP"),
        ("JSX", "JavaScript (Babel)"),
        ("Lean", "Lean 4"),
        ("Lean4", "Lean 4"),
        ("LessCss", "Less"),
        ("Matlab", "MATLAB"),
        ("NASM", "x86_64 Assembly"),
        ("NewLisp", "Lisp"),
        ("Nginx configuration file", "nginx"),
        ("Nimrod", "Nim"),
        ("NumPy", "Python"),
        ("Octave", "MATLAB"),
        ("Org Mode", "orgmode"),
        ("Properties", "Java Properties"),
        ("Python 2.x", "Python"),
        ("RHTML", "HTML (Rails)"),
        ("Rd", "Rd (R Documentation)"),
        ("RobotFramework", "Robot Framework"),
        ("S", "R"),
        ("Scheme", "Lisp"),
        ("Slim", "Ruby Slim"),
        ("SQL+Jinja", "SQL"),
        ("Standard ML", "SML"),
        ("TASM", "x86_64 Assembly"),
        ("TeX", "LaTeX"),
        ("Todotxt", "Todo.txt"),
        ("Transact-SQL", "SQL"),
        ("TSX", "TypeScriptReact"),
        ("Vue", "Vue Component"),
        ("WebGPU Shading Language", "WGSL"),
        ("XML+Django/Jinja", "Jinja2"),
        ("XSLT", "XML"),
        ("YAML+Jinja", "YAML"),
        ("reg", "INI"),
    ];
    Some(SAME_LANGUAGE.iter().find(|(p, _)| *p == pygments).map_or(pygments, |(_, s)| s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(name: &str, text: &str) -> String {
        language_for(name, text).0
    }

    #[test]
    fn names_are_pygments_names() {
        // Checked against 1.x (`_lexer_for`).
        for (file, want) in [
            ("a.py", "Python"),
            ("a.sh", "Bash"),
            (".bashrc", "Bash"),
            ("a.js", "JavaScript"),
            ("a.jsx", "JSX"),
            ("a.tsx", "TSX"),
            ("Dockerfile", "Docker"),
            ("a.bat", "Batchfile"),
            ("a.properties", "Properties"),
            ("a.vue", "Vue"),
            ("a.tex", "TeX"),
            ("a.less", "LessCss"),
            ("Makefile", "Makefile"),
            ("Makefile.am", "Makefile"),
            ("CMakeLists.txt", "CMake"),
            ("a.html.j2", "HTML+Django/Jinja"),
            ("a.1", "Groff"),
            ("nginx.conf", "Nginx configuration file"),
            ("a.txt", "Plain text"),
            ("a.csv", "Plain text"),
            ("a.log", "Plain text"),
            ("a.conf", "Plain text"),
            (".gitignore", "Plain text"),
            (".env", "Plain text"),
            ("README", "Plain text"),
        ] {
            assert_eq!(lang(file, "x\n"), want, "{file}");
        }
    }

    #[test]
    fn the_text_breaks_ties() {
        assert_eq!(lang("a.h", "#include <stdio.h>\nint f(void);\n"), "C");
        assert_eq!(lang("a.h", "#ifndef A_H\n#define A_H\n#endif\n"), "C");
        assert_eq!(lang("a.h", "int f(void);\n"), "Objective-C", "Pygments' tie-break by class name");
        assert_eq!(lang("a.h", ""), "C", "an empty file: by priority");
        assert_eq!(lang("a.m", "#import \"A.h\"\n@implementation A\n@end\n"), "Objective-C");
        assert_eq!(lang("a.m", "% comment\nfunction y = f(x)\nend\n"), "Matlab");
        assert_eq!(lang("a.m", "x = 1;\n"), "Octave");
        assert_eq!(lang("a.pl", "use strict;\nmy $x = 1;\n"), "Perl");
        assert_eq!(lang("a.pl", "foo(X) :- bar(X).\n"), "Prolog");
        assert_eq!(lang("a.pm", "package Foo;\nuse strict;\n1;\n"), "Promela");
        assert_eq!(lang("a.sql", "SELECT * FROM t;\n"), "Transact-SQL");
        assert_eq!(lang("a.fs", "let x = 1\n"), "Forth");
        assert_eq!(lang("a.s", ".text\nmov r0, r1\n"), "GAS");
        assert_eq!(lang("a.asm", "section .text\n"), "TASM");
        assert_eq!(lang("a.t", "use Test::More;\n"), "TADS 3");
        // Checked against 1.x as well.
        assert_eq!(lang("a.sql", "select * from {{ ref('orders') }}\n"), "SQL+Jinja", "dbt");
        assert_eq!(lang("a.i", "%module example\n%{\n#include \"example.h\"\n%}\n"), "SWIG");
        assert_eq!(lang("a.i", "class predicates\n  foo : ().\n"), "Visual Prolog");
        assert_eq!(lang("a.xsl", "<?xml version=\"1.0\"?>\n<root><a>1</a></root>\n"), "XML");
        assert_eq!(lang("a.xsl", "<?xml version=\"1.0\"?>\n<xsl:stylesheet></xsl:stylesheet>\n"), "XSLT");
        assert_eq!(lang("a.xslt", "<!DOCTYPE html>\n<html><body></body></html>\n"), "HTML");
        assert_eq!(lang("a.xslt", "<root><a>1</A></root>\n"), "XML", "a tag and its end tag");
        assert_eq!(lang("a.j", ".class public Foo\n.super java/lang/Object\n"), "Jasmin");
        assert_eq!(lang("a.j", "@import <Foundation/CPObject.j>\n"), "Objective-J");
        assert_eq!(lang("a.lean", "import data.nat.basic\n"), "Lean");
        assert_eq!(lang("a.lean", "import Mathlib\n"), "Lean4");
        assert_eq!(lang("a.mi", "f := proc(x) x^2 end proc;\n"), "Maple");
        assert_eq!(lang("a.scd", "foo(1)\n\n# NAME\n"), "scdoc");
        assert_eq!(lang("a.def", "DEFINITION MODULE M;\n(* x *)\nPROCEDURE P(VAR a := 1);\nEND M.\n"), "Modula-2");
        assert_eq!(lang("a.def", "Bootstrap: docker\n%post\n  apt-get update\n"), "Singularity");
    }

    #[test]
    fn scripts_by_interpreter() {
        for (first, want) in [
            ("#!/bin/sh", "Bash"),
            ("#! /bin/sh", "Bash"),
            ("#!/bin/bash -e", "Bash"),
            ("#!/usr/bin/env python3", "Python"),
            ("#!/usr/bin/env -S python3 -u", "Python"),
            ("#!C:\\Python\\python.exe", "Python"),
            ("#!/usr/bin/python2", "Python 2.x"),
            ("#!/usr/bin/env python3.12", "Plain text"),
            ("#!/usr/bin/perl", "Perl"),
            ("#!/usr/local/bin/ruby1.9", "Ruby"),
            ("#!/usr/bin/env php", "JavaScript+PHP"),
            ("#!/usr/bin/env node", "Plain text"),
            ("#!/usr/bin/env lua", "Plain text"),
        ] {
            assert_eq!(lang("run", &format!("{first}\nx\n")), want, "{first}");
        }
        assert_eq!(lang("run", "#!/usr/bin/env node\nimport x from 'y'\n"), "Python", "as 1.x guessed");
        assert_eq!(lang("run", "#!/usr/bin/env python3\nimport numpy as np\n"), "NumPy");
        assert_eq!(lang("run", "#!/usr/bin/env ruby\nimport x\n"), "Python", "Python's lexer is tried before Ruby's");
        assert_eq!(lang("run", "#!/usr/bin/env perl\nimport x\n"), "Perl", "and after Perl's");
        assert_eq!(lang("run", "no shebang\n"), "Plain text");
    }

    #[test]
    fn languages_highlight_with_their_syntect_syntax() {
        let set = oc_preview::highlight::syntax_set();
        for file in ["a.py", "a.sh", "a.js", "a.h", "a.c", "a.cpp", "a.tsx", "a.sql", "a.rs", "Dockerfile", "a.tex", "a.md", "a.json"] {
            let (lang, syntax) = language_for(file, "x\n");
            let syntax = syntax.unwrap_or_else(|| panic!("{file}: no syntax for {lang}"));
            assert!(set.syntaxes().iter().any(|s| s.name.eq_ignore_ascii_case(syntax)), "{file}: {syntax} not in syntect");
        }
        assert_eq!(language_for("a.csv", "a,b\n").1, None);
    }
}
