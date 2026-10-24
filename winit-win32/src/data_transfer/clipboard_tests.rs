use super::*;

/// The `[StartHTML, EndHTML, StartFragment, EndFragment]` offsets of an HTML Clipboard Format
/// buffer built by `build_html_clipboard_format`.
fn html_offsets(buf: &[u8]) -> [usize; 4] {
    let text = std::str::from_utf8(buf).unwrap();
    ["StartHTML:", "EndHTML:", "StartFragment:", "EndFragment:"].map(|name| {
        let at = text.find(name).unwrap() + name.len();
        text[at..at + 10].parse().unwrap()
    })
}

fn html_fragment(buf: &[u8]) -> &str {
    let [_, _, start, end] = html_offsets(buf);
    std::str::from_utf8(&buf[start..end]).unwrap()
}

#[test]
fn html_with_a_start_marker_but_no_end_marker_is_wrapped() {
    let html = "<p>a</p><!--StartFragment--><p>b</p>";
    let buf = build_html_clipboard_format(html);
    assert_eq!(html_fragment(&buf), html);
}

#[test]
fn html_with_the_end_marker_before_the_start_marker_is_wrapped() {
    let html = "<!--EndFragment--><p>a</p><!--StartFragment-->";
    let buf = build_html_clipboard_format(html);
    let [start_html, end_html, start_fragment, end_fragment] = html_offsets(&buf);
    assert!(start_html <= start_fragment);
    assert!(start_fragment <= end_fragment);
    assert!(end_fragment <= end_html);
    assert_eq!(end_html, buf.len());
    assert_eq!(html_fragment(&buf), html);
}

#[test]
fn html_with_its_own_fragment_markers_is_kept_as_is() {
    let html = "<html><body>x<!--StartFragment--><b>y</b><!--EndFragment-->z</body></html>";
    let buf = build_html_clipboard_format(html);
    let [start_html, end_html, ..] = html_offsets(&buf);
    assert_eq!(&buf[start_html..end_html], html.as_bytes());
    assert_eq!(html_fragment(&buf), "<b>y</b>");
}

#[test]
fn html_without_fragment_markers_is_wrapped() {
    let html = "<b>y</b><!--EndFragment-->";
    let buf = build_html_clipboard_format(html);
    let [start_html, end_html, ..] = html_offsets(&buf);
    assert_eq!(end_html, buf.len());
    let document = std::str::from_utf8(&buf[start_html..end_html]).unwrap();
    assert!(document.starts_with("<html><body>"));
    assert!(document.ends_with("</body></html>"));
    assert_eq!(html_fragment(&buf), html);
}
