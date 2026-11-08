use crate::tui::screen;
use mmdflux::{render_diagram, OutputFormat, RenderConfig};

/// mmdflux panics on malformed nested subgraphs
pub(crate) fn drawn(code: &str) -> Option<Vec<String>> {
    let config = RenderConfig::default();
    let drawing = || render_diagram(code, OutputFormat::Text, &config);
    let drawing = screen::caught(drawing)?.ok()?;
    let lines = drawing.lines().map(|line| line.trim_end().to_owned());
    Some(lines.collect())
}

pub(crate) fn page(code: &str) -> Result<String, String> {
    let config = RenderConfig::default();
    let drawing = || render_diagram(code, OutputFormat::Svg, &config);
    let svg = match screen::caught(drawing) {
        Some(Ok(svg)) => svg,
        Some(Err(e)) => return Err(format!("diagram: {e}")),
        None => return Err("diagram: the renderer crashed".to_owned()),
    };

    let html = format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>diagram</title>\n<body style=\"margin: 16px; background: #fff\">\n{svg}\n</body>\n"
    );
    Ok(html)
}

#[cfg(test)]
mod tests {
    use crate::tui::diagram::page;

    #[test]
    fn page_wraps_svg() {
        let code = "graph LR\n    A[read 🐍] --> B[write 🚀]";
        let html = page(code).expect("page");
        assert!(html.contains("<svg") && html.contains("read 🐍"), "{html}");

        let refused = page("graph TD\n    A[unclosed --> B");
        assert!(refused.is_err_and(|e| e.starts_with("diagram: ")));
    }
}
