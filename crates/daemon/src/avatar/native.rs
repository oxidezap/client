use anyhow::{Context, Result};

pub(super) fn fetch(url: &str) -> Result<(u16, Vec<u8>)> {
    let response = ureq::get(url).call().context("fetching avatar")?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .read_to_vec()
        .context("reading avatar")?;
    Ok((status, body))
}
