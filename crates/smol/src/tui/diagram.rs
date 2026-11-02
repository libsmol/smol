use crate::tui::{browser, screen};
use mmdflux::{render_diagram, OutputFormat, RenderConfig};
use reaper::cause;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;

/// mmdflux panics on malformed nested subgraphs
pub(crate) fn drawn(code: &str) -> Option<Vec<String>> {
    let config = RenderConfig::default();
    let drawing = || render_diagram(code, OutputFormat::Text, &config);
    let drawing = screen::caught(drawing)?.ok()?;
    let lines = drawing.lines().map(|line| line.trim_end().to_owned());
    Some(lines.collect())
}

/// The same diagram always goes to the same file
pub(crate) fn page(code: &str) -> Result<PathBuf, String> {
    let config = RenderConfig::default();
    let drawing = || render_diagram(code, OutputFormat::Svg, &config);
    let svg = match screen::caught(drawing) {
        Some(Ok(svg)) => svg,
        Some(Err(e)) => return Err(format!("diagram: {e}")),
        None => return Err("diagram: the renderer crashed".to_owned()),
    };

    let mut hasher = DefaultHasher::new();
    code.hash(&mut hasher);
    let hash = hasher.finish();
    let name = format!("smol-diagram-{hash:016x}.html");
    let path = browser::private_dir().join(name);

    let html = format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>diagram</title>\n<body style=\"margin: 16px; background: #fff\">\n{svg}\n</body>\n"
    );
    let failed = |e: io::Error| {
        let reason = cause(&e);
        format!("diagram: {}: {reason}", path.display())
    };
    browser::write_private(&path, html.as_bytes()).map_err(failed)?;

    Ok(path)
}

pub(crate) fn open(code: &str) -> Result<(), String> {
    let path = page(code)?;
    browser::open(&path)
}

#[cfg(test)]
mod tests {
    use crate::tui::diagram::{drawn, page};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn drawn_and_paged() {
        let code = "graph LR\n    A[read 🐍] --> B[write 🚀]";
        let lines = drawn(code).expect("drawn");
        assert!(
            lines.iter().any(|line| line.contains("│ read 🐍 │")),
            "{lines:?}"
        );
        assert_eq!(drawn("pie title Share\n    \"Python\" : 40"), None);

        let path = page(code).expect("page");
        let html = fs::read_to_string(&path).expect("read");
        assert!(html.contains("<svg") && html.contains("read 🐍"), "{html}");
        assert_eq!(page(code), Ok(path.clone()));
        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        fs::remove_file(&path).ok();

        let refused = page("graph TD\n    A[unclosed --> B");
        assert!(refused.is_err_and(|e| e.starts_with("diagram: ")));
    }
}
