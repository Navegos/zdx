mod cli;
mod modes;

use zdx_engine::core::interrupt;

fn main() {
    if let Err(e) = cli::run() {
        if e.downcast_ref::<interrupt::InterruptedError>().is_some() {
            std::process::exit(130);
        }
        if e.downcast_ref::<zdx_engine::jobs::store::JobBusy>()
            .is_some()
        {
            eprintln!("{e:#}");
            std::process::exit(zdx_engine::jobs::store::JobBusy::EXIT_CODE);
        }
        eprintln!("{e:#}"); // pretty anyhow chain
        std::process::exit(1);
    }
}
