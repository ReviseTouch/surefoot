//! Say what each path is standing on.

fn main() {
    for path in std::env::args().skip(1) {
        let p = surefoot::probe::probe(std::path::Path::new(&path));
        println!(
            "{path}: {} on {}, disk {}, case-sensitive {}, real modes {}",
            p.filesystem.as_deref().unwrap_or("?"),
            p.medium.label(),
            p.disk.as_deref().unwrap_or("?"),
            p.traits.case_sensitive,
            p.traits.real_modes,
        );
    }
}
