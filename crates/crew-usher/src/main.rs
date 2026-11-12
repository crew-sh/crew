use std::time::Duration;

use anyhow::Context;
use crew_depot_config::DepotTable;
use crew_sys::signals;
use crew_usher::config::Config;
use crew_usher::server::{IdleSource, Paths, Server};

fn main() -> anyhow::Result<()> {
    let config_path = crew_sys::paths::config_dir().join("usher.toml");
    let mut cfg =
        Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;
    cfg.depot = DepotTable::user().context("loading depot.toml")?.resolve();
    let paths = Paths::from_env();
    signals::install_termination_handler();
    let server = Server::start(cfg, &paths, IdleSource::Wayland, true)?;
    eprintln!("crew-usher: listening on {}", paths.socket.display());
    while signals::take_pending().is_none() && server.is_running() {
        std::thread::sleep(Duration::from_millis(200));
    }
    let failed = !server.is_running();
    server.stop();
    // A usher whose actor or acceptor died serves nobody; exit so the service manager restarts it.
    if failed {
        anyhow::bail!("the usher's actor or acceptor thread ended");
    }
    Ok(())
}
