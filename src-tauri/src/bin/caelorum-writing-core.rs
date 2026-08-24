fn main() {
    if let Err(error) = caelorum_writing_spike_lib::run_headless() {
        eprintln!("[CORE] fatal: {error}");
        std::process::exit(1);
    }
}
