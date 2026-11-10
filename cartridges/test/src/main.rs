fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdin = std::io::stdin();
    let code = echo_cart::run(&args, stdin.lock(), std::io::stdout());
    std::process::exit(code);
}
