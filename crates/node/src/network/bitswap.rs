use async_trait::async_trait;
use futures::prelude::*;
use libp2p::{
    request_response::{self, Codec},
    StreamProtocol,
};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration;

pub const BSWP_PROTOCOL: &str = "/pom/bitswap/1.0.0";

/// Manifest describing a file available for transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileManifest {
    pub file_name: String,
    pub total_size: u64,
    pub chunk_size: u64,
    pub num_chunks: u32,
    pub chunks: Vec<ChunkInfo>,
    pub md5_full: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkInfo {
    pub index: u32,
    pub offset: u64,
    pub size: u64,
    pub sha256: String,
}

/// Bitswap protocol messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BitswapRequest {
    WantList {
        manifest_sha256: String,
        chunk_ids: Vec<u32>,
    },
    GetManifest {
        file_name: String,
    },
    Cancel {
        chunks: Vec<u32>,
    },
    Have {
        chunks: Vec<(u32, String)>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BitswapResponse {
    Block { chunk_index: u32, data: Vec<u8> },
    Manifest(FileManifest),
    Ack,
    Error { message: String },
}

#[derive(Debug, Clone, Default)]
pub struct BitswapCodec;

async fn read_msg<T, R>(io: &mut R) -> io::Result<T>
where
    T: serde::de::DeserializeOwned,
    R: AsyncRead + Unpin + Send,
{
    use futures::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut data = vec![0u8; len];
    io.read_exact(&mut data).await?;
    serde_json::from_slice(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

async fn write_msg<T, W>(io: &mut W, msg: &T) -> io::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin + Send,
{
    use futures::AsyncWriteExt;
    let data = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    io.write_all(&(data.len() as u32).to_be_bytes()).await?;
    io.write_all(&data).await?;
    Ok(())
}

#[async_trait]
impl Codec for BitswapCodec {
    type Protocol = StreamProtocol;
    type Request = BitswapRequest;
    type Response = BitswapResponse;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io).await
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io).await
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &req).await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &res).await
    }
}

/// Create a request_response::Behaviour for bitswap.
pub fn bitswap_behaviour() -> request_response::Behaviour<BitswapCodec> {
    request_response::Behaviour::new(
        [(
            StreamProtocol::new(BSWP_PROTOCOL),
            request_response::ProtocolSupport::Full,
        )],
        request_response::Config::default().with_request_timeout(Duration::from_secs(60)),
    )
}

// Os digests do transporte de arquivo vivem em `backend/transfer.rs`, que é
// quem move bytes de verdade (HTTP com Range). As cópias que existiam aqui não
// tinham chamador nenhum fora dos próprios testes.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_serialization() {
        let m = FileManifest {
            file_name: "test.gguf".into(),
            total_size: 1024,
            chunk_size: 256,
            num_chunks: 4,
            chunks: vec![ChunkInfo {
                index: 0,
                offset: 0,
                size: 256,
                sha256: "abc".into(),
            }],
            md5_full: "def".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: FileManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.file_name, "test.gguf");
        assert_eq!(parsed.num_chunks, 4);
    }
}
