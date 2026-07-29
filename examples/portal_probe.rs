//! Minimal Portal screenshot probe: no GTK, just calls the Portal via ashpd
//! and prints the result. Used to diagnose GNOME Portal's real response to an
//! ashpd call (dialog vs immediate Other).
//!
//! Run: cargo run --example portal_probe

use ashpd::desktop::screenshot::Screenshot;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // GLINT_INTERACTIVE=1 uses interactive=true to test what UI GNOME shows
    let interactive = std::env::var("GLINT_INTERACTIVE").ok().as_deref() == Some("1");
    log::info!("portal_probe starting, interactive={interactive}, requesting screenshot...");
    async_std::task::block_on(async {
        let response = Screenshot::request()
            .interactive(interactive)
            .modal(true)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Portal request failed: {e}"))?
            .response();
        match response {
            Ok(s) => {
                let uri = s.uri();
                log::info!("Screenshot OK! URI = {uri}");
            }
            Err(e) => {
                log::error!("Screenshot rejected: {e}");
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}
