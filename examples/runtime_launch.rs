use std::error::Error;

use browserkit::runtime::{IsolatedSessionOptions, LaunchOptions};
use browserkit::{BrowserRuntime, CloseReport};

fn require_complete(resource: &str, report: CloseReport) -> Result<(), Box<dyn Error>> {
    if report.is_complete() {
        Ok(())
    } else {
        Err(format!("{resource} did not close completely: {report:#?}").into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let runtime = BrowserRuntime::launch(LaunchOptions::default().headless(true)).await?;
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await?;
    let page = session.new_page("about:blank").await?;
    let frame = page.main_frame().await?;

    let _browser_protocol = runtime.cdp();
    let _page_protocol = page.cdp_session();
    let frame_protocol = frame.cdp_session().await?;
    println!("launched frame session: {}", frame_protocol.id());

    require_complete("created page", page.close().await)?;
    require_complete("isolated session", session.close().await)?;
    require_complete("runtime", runtime.close().await)
}
