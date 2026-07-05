fn main() {
    let (ok, report) = phase0_spike::run_spike();
    print!("{report}");
    std::process::exit(if ok { 0 } else { 1 });
}
