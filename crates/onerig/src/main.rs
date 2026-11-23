fn main() {
    // shortcut: no commands yet, the core library is built first; wire `run`,
    // `resume` and `doctor` here once `onerig_core::run` exists.
    eprintln!("onerig {}: nothing to run yet", env!("CARGO_PKG_VERSION"));
    std::process::exit(2);
}
