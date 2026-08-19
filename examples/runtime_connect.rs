use std::error::Error;

use browserkit::{BrowserRuntime, CloseReport};
use cdpkit::target::methods::GetTargets;

fn require_complete(resource: &str, report: CloseReport) -> Result<(), Box<dyn Error>> {
    if report.is_complete() {
        Ok(())
    } else {
        Err(format!("{resource} did not close completely: {report:#?}").into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let endpoint = std::env::args()
        .nth(1)
        .ok_or("usage: cargo run --example runtime_connect -- <host:port|http://...|ws://...>")?;
    let runtime = BrowserRuntime::connect(endpoint).await?;
    let session = runtime.default_session().await?;

    let targets = GetTargets::new().send(runtime.cdp()).await?;
    let target_id = targets
        .target_infos
        .into_iter()
        .find(|target| target.type_ == "page" && target.subtype.is_none())
        .map(|target| target.target_id);
    let Some(target_id) = target_id else {
        require_complete("default session", session.close().await)?;
        require_complete("runtime", runtime.close().await)?;
        return Err("the connected browser has no normal page target".into());
    };

    let page = session.attach_page(target_id).await?;
    let frame = page.main_frame().await?;
    let _browser_protocol = runtime.cdp();
    let _page_protocol = page.cdp_session();
    let frame_protocol = frame.cdp_session().await?;
    println!(
        "page={} frame={} frame_session={}",
        page.id(),
        frame.id(),
        frame_protocol.id()
    );

    require_complete("attached page", page.close().await)?;
    require_complete("default session", session.close().await)?;
    require_complete("runtime", runtime.close().await)
}
