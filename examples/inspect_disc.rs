//! Quick CLI to inspect any supported disc image — container, filesystem,
//! game-console identity, and a top-level directory listing.
//!
//! Usage: `cargo run --example inspect_disc -- <path-to-image>`

use opticaldiscs::browse;
use opticaldiscs::detect::DiscImageInfo;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: inspect_disc <path-to-image>")?;
    let info = DiscImageInfo::open(&path)?;

    println!("Path        : {}", info.path.display());
    println!("Format      : {}", info.format.display_name());
    println!("Filesystem  : {}", info.filesystem.display_name());
    println!("Volume label: {:?}", info.volume_label);

    if let Some(g) = &info.game {
        println!("\n── Game disc ──");
        println!("Console     : {}", g.console.display_name());
        println!("Serial      : {:?}", g.serial);
        println!("Title       : {:?}", g.title);
        println!("Region      : {:?}", g.region.map(|r| r.display_name()));
        println!("Maker       : {:?}", g.maker);
        println!("Version     : {:?}", g.version);
    }

    if !info.filesystem.is_browsable() {
        println!("\n(filesystem not browsable yet)");
        return Ok(());
    }

    let mut fs = browse::open_disc_filesystem(&info)?;
    let root = fs.root()?;
    println!("\n/ ({}):", fs.volume_name().unwrap_or("(no label)"));
    let mut entries = fs.list_directory(&root)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries.iter().take(40) {
        let kind = if e.is_directory() { "d" } else { "-" };
        println!("  {kind} {:<40} {}", e.name, e.size_string());
    }
    if entries.len() > 40 {
        println!("  … {} more", entries.len() - 40);
    }
    Ok(())
}
