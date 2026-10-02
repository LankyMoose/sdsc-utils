//! Pack `sdsc-shell` onto the end of `sdsc-utils` for the portable download.

use std::env;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let (Some(service), Some(shell), Some(out)) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: bundle-portable <sdsc-utils> <sdsc-shell> <out>");
        return ExitCode::from(2);
    };
    match sdsc_utils::platform::shell_bundle::append_shell_bundle(
        Path::new(&service),
        Path::new(&shell),
        Path::new(&out),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
