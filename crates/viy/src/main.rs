fn main() {
    viy_cli::hardening::pre_main();
    let code = viy_cli::run(
        std::env::args_os().skip(1),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        viy_cli::Terminal::detect(),
    );
    std::process::exit(code);
}
