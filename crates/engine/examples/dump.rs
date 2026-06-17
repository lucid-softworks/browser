//! Diagnostic: measure reveal (display:none before/after JS) + remaining errors.
//! Usage: cargo run -p engine --example dump -- <file.html> <base-url>

fn none_count(doc: &dom::Document, base: &str) -> (usize, usize) {
    let (sheets, _n) = engine::collect_stylesheets(doc, base);
    let computed = style::cascade(doc, &sheets);
    (computed.len(), computed.values().filter(|c| c.display_none).count())
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump <file.html> <base>");
    let base = std::env::args().nth(2).unwrap_or_else(|| "https://example.com/".into());
    let bytes = std::fs::read(&path).expect("read file");
    let html = String::from_utf8_lossy(&bytes);

    let doc = html::parse(&html);
    let (n, none_before) = none_count(&doc, &base);
    println!("BEFORE: {n} elements, {none_before} display:none");

    let (doc, console) = engine::run_scripts(doc, &base);
    let errs: Vec<&String> = console.iter().filter(|l| l.starts_with('\u{26a0}')).collect();
    let (_n, none_after) = none_count(&doc, &base);
    println!("AFTER (classic):  {none_after} display:none  (revealed {})", none_before as i64 - none_after as i64);
    println!("classic errors: {}", errs.len());
    for e in &errs {
        println!("{}", e.lines().next().unwrap_or(""));
        for l in e.lines().skip(1).take(2) {
            println!("{l}");
        }
    }

    // --- ES modules (deferred) ---
    let (entries, sources, notes) = engine::collect_module_graph(&doc, &base);
    println!(
        "\nMODULE GRAPH: {} entries, {} modules fetched, {} notes",
        entries.len(),
        sources.len(),
        notes.len()
    );
    for n in notes.iter().take(20) {
        println!("  {n}");
    }
    if !entries.is_empty() {
        let (doc, mconsole) = engine::run_modules(doc, &base);
        let merrs: Vec<&String> = mconsole.iter().filter(|l| l.starts_with('\u{26a0}')).collect();
        let logs: Vec<&String> = mconsole.iter().filter(|l| !l.starts_with('\u{26a0}') && !l.starts_with('[')).collect();
        let (_n2, none_mod) = none_count(&doc, &base);
        println!("AFTER (modules):  {none_mod} display:none");
        println!("module console logs: {}", logs.len());
        for l in logs.iter().take(20) {
            println!("  log: {l}");
        }
        println!("module errors: {}", merrs.len());
        for e in merrs.iter().take(20) {
            println!("  ERR: {e}");
        }
    }
}
