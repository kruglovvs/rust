//! Device entry point: hand control to the pipeline orchestrator.

mod app;
#[cfg(feature = "rtsp-selftest")]
mod selftest;

const RUNTIME_STACK_BYTES: usize = 64 * 1024;

fn main() {
    let spawned = std::thread::Builder::new()
        .name("runtime".into())
        .stack_size(RUNTIME_STACK_BYTES)
        .spawn(app::run);

    match spawned {
        Ok(handle) => {
            let _ = handle.join();
        }
        Err(error) => {
            println!("boot: FATAL: cannot spawn runtime thread: {error}");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    }
}
