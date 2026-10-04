fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("cinder {}", cinder::VERSION);
        return;
    }
    eprintln!("cinder: error: no input files");
    std::process::exit(1);
}
