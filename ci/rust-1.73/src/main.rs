// Detects a disc image's format, so the job proves the 1.73 build links and runs.
fn main() {
    let path = std::env::args().nth(1).expect("a disc image path");
    let format = opticaldiscs::formats::DiscFormat::from_path(&path);
    println!("{path}: {format:?}");
}
