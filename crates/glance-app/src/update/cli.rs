//! `glance update [--apply] [--restart]`: check for an update from a
//! terminal (headless setups, and testing the updater end to end).
//! Without `--apply` it only checks and downloads; `--apply` installs the
//! staged update; `--restart` then starts the new version with
//! `--after-update --version` (it prints its version and exits).

use super::{Config, Status, apply};

pub const USAGE: &str = "usage: glance update [--apply] [--restart]";

pub fn run(args: &[String]) -> i32 {
    let apply_it = args.iter().any(|a| a == "--apply");
    let restart = args.iter().any(|a| a == "--restart");
    if let Some(bad) = args
        .iter()
        .find(|a| !matches!(a.as_str(), "--apply" | "--restart" | "--check"))
    {
        eprintln!("glance update: unknown argument {bad}\n{USAGE}");
        return 2;
    }
    let cfg = Config::from_env(&crate::net::data_dir());
    println!(
        "glance {} ({}){}",
        cfg.current,
        cfg.kind.label(),
        cfg.kind
            .target()
            .map(|t| format!(": {}", t.display()))
            .unwrap_or_default()
    );
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("glance update: {e}");
            return 1;
        }
    };
    let result = rt.block_on(super::check_and_stage(&cfg, |s| {
        if !matches!(s, Status::Downloading(_, pct) if pct % 10 != 0) {
            println!("{}", s.text());
        }
    }));
    let status = match result {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e:#}");
            return 1;
        }
    };
    println!("{}", status.text());
    let Status::Ready(staged) = status else {
        return match status {
            Status::UpToDate | Status::Available(..) | Status::Unavailable(_) => 0,
            _ => 1,
        };
    };
    println!("staged at {}", staged.path.display());
    if !apply_it {
        return 0;
    }
    match apply::apply(&staged) {
        Ok(apply::Next::Launch(exe)) => {
            println!("updated to v{}", staged.version);
            if restart && let Err(e) = apply::relaunch(&exe, &[apply::AFTER_UPDATE, "--version"]) {
                eprintln!("Error: {e:#}");
                return 1;
            }
            0
        }
        Ok(apply::Next::InstallerRelaunches) => {
            println!("the installer is updating glance to v{}", staged.version);
            0
        }
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    }
}
