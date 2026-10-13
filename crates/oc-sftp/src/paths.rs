//! POSIX path helpers with Python `posixpath` semantics (server paths are
//! absolute POSIX paths whatever the local OS is).

/// `posixpath.join(path, *names)`; empty names are skipped.
pub fn join(path: &str, names: &[&str]) -> String {
    let mut out = path.to_string();
    for name in names.iter().filter(|n| !n.is_empty()) {
        if name.starts_with('/') {
            out = (*name).to_string();
        } else if out.is_empty() || out.ends_with('/') {
            out.push_str(name);
        } else {
            out.push('/');
            out.push_str(name);
        }
    }
    out
}

/// `posixpath.dirname(path)`.
pub fn dirname(path: &str) -> &str {
    let i = path.rfind('/').map_or(0, |i| i + 1);
    let head = &path[..i];
    if !head.is_empty() && head.bytes().any(|b| b != b'/') { head.trim_end_matches('/') } else { head }
}

/// `posixpath.basename(path)`.
pub fn basename(path: &str) -> &str {
    path.rfind('/').map_or(path, |i| &path[i + 1..])
}

/// `posixpath.normpath(path)`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    // POSIX allows one implementation-defined leading "//"; three or more are one "/".
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if initial == 0 && parts.last().is_none_or(|p| *p == "..") {
                    parts.push("..");
                } else if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                }
                // ".." at the root of an absolute path stays at the root.
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("/");
    let out = format!("{}{joined}", "/".repeat(initial));
    if out.is_empty() { ".".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_posixpath() {
        assert_eq!(join("/", &["docs"]), "/docs");
        assert_eq!(join("/a", &["b", "c"]), "/a/b/c");
        assert_eq!(join("/a", &["/abs"]), "/abs");
        assert_eq!(join("/a/", &["", "b"]), "/a/b");
        assert_eq!(dirname("/docs"), "/");
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("/a/b/"), "/a/b");
        assert_eq!(dirname("//a"), "//");
        assert_eq!(dirname("a"), "");
        assert_eq!(basename("/a/b"), "b");
        assert_eq!(basename("/"), "");
        assert_eq!(normpath("/home/u/../x/./y//"), "/home/x/y");
        assert_eq!(normpath("/../x"), "/x");
        assert_eq!(normpath("a/../../b"), "../b");
        assert_eq!(normpath("//x"), "//x");
        assert_eq!(normpath("///x"), "/x");
        assert_eq!(normpath(""), ".");
        assert_eq!(normpath("/"), "/");
    }
}
