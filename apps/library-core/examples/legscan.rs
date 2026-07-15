use library_core::legibility::{legibility, min_window, squash_ws};

fn main() {
    for arg in std::env::args().skip(1) {
        let (path, page) = arg.split_once(':').expect("file.md:page");
        let md = std::fs::read_to_string(path).expect("read");
        let marker = format!("<!-- page {page} -->");
        let next = "<!-- page ";
        let start = md.find(&marker).map(|i| i + marker.len()).unwrap_or(0);
        let rest = &md[start..];
        let end = rest.find(next).unwrap_or(rest.len());
        let text = squash_ws(&rest[..end]);
        let head: String = text.chars().take(60).collect();
        println!(
            "{:5.3} min {:5.3}  {}:{}  {}",
            legibility(&text),
            min_window(&text),
            path,
            page,
            head
        );
    }
}
