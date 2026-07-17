fn main() {
    let code = viy_cli::run(
        std::env::args_os().skip(1),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
