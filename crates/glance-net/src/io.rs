//! Reading and writing glance frames on iroh streams.

use anyhow::{Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Serialize, de::DeserializeOwned};

pub async fn write_frame<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let frame = glance_proto::encode(msg)?;
    send.write_all(&frame).await.context("write frame")?;
    Ok(())
}

pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let mut prefix = [0u8; 4];
    recv.read_exact(&mut prefix)
        .await
        .context("read frame length")?;
    let len = glance_proto::frame::body_len(prefix)?;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .context("read frame body")?;
    Ok(glance_proto::decode(&body)?)
}
