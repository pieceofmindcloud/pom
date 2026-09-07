use anyhow::Result;

pub fn run<F>(_: u16, service: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    service()
}
