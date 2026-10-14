/// Shell-style matching: `*` is any run of characters and `?` any one,
/// neither crossing a `/`.
pub fn matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti] || (p[pi] == '?' && t[ti] != '/')) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            if t[st] == '/' {
                return false;
            }
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn basics() {
        assert!(matches("models-*", "models-dev"));
        assert!(matches("models-*", "models-"));
        assert!(!matches("models-*", "model-dev"));
        assert!(matches("copy-*", "copy-"));
        assert!(matches("copy-*", "copy-2"));
        assert!(matches("remotion-v4.*", "remotion-v4.0.355"));
        assert!(!matches("remotion-v4.*", "remotion-v40"));
        assert!(matches("*Tests-x", "WhatsNewTests-x"));
        assert!(matches("/a/*/c", "/a/b/c"));
        assert!(!matches("/a/*/c", "/a/b/x/c"));
        assert!(matches("models", "models"));
        assert!(!matches("models", "models2"));
        assert!(matches("a?c", "abc"));
    }
}
