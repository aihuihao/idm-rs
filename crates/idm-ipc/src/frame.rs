//! Firefox Native Messaging 帧编解码（独立模块，便于单元测试）。
//!
//! 每条消息前 4 字节为消息长度（本机字节序，Windows 上为小端），后面是 UTF-8 JSON。

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 单条消息长度上限：64 KiB。超过则在读取负载前即拒绝，绝不先分配大缓冲区。
pub const MAX_MESSAGE_LEN: usize = 64 * 1024;

/// 读取一帧的结果。
#[derive(Debug)]
pub enum ReadFrame {
    /// 读到一条完整消息（负载，不含长度前缀）。
    Message(Vec<u8>),
    /// 标准输入关闭（包括首字节即为 EOF，或只读到半截长度/负载后关闭）。
    Eof,
    /// 消息长度超过 [`MAX_MESSAGE_LEN`]，调用方应返回错误并关闭。
    TooLarge,
}

/// 从 `reader` 读取一帧。
///
/// - 起始即 EOF 或半截数据后关闭：返回 [`ReadFrame::Eof`]。
/// - 长度超过上限：返回 [`ReadFrame::TooLarge`]，**在此之前不分配大于上限的缓冲区**。
/// - 正常：返回 [`ReadFrame::Message`]。
pub async fn read_frame<R>(reader: &mut R) -> std::io::Result<ReadFrame>
where
    R: AsyncReadExt + Unpin,
{
    let mut lenbuf = [0u8; 4];
    match reader.read_exact(&mut lenbuf).await {
        Ok(_) => {}
        // 无论读到的字节数为 0 还是半截，统一视为对端关闭。
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(ReadFrame::Eof),
        Err(e) => return Err(e),
    }
    let len = u32::from_ne_bytes(lenbuf) as usize;
    if len > MAX_MESSAGE_LEN {
        // 先检查长度，再分配缓冲区——绝不先分配 64 KiB 以上的缓冲。
        return Ok(ReadFrame::TooLarge);
    }
    let mut payload = vec![0u8; len];
    match reader.read_exact(&mut payload).await {
        Ok(_) => Ok(ReadFrame::Message(payload)),
        // 长度读到了，但负载在传完前被截断：同样视为对端关闭。
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(ReadFrame::Eof),
        Err(e) => Err(e),
    }
}

/// 向 `writer` 写一帧：`[4 字节本机字节序长度][UTF-8 JSON]`。
///
/// 序列化失败时返回 `Err` 且**不写出任何字节**：绝不发送长度为 0 的空帧。
pub async fn write_frame<W, V>(writer: &mut W, value: &V) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    V: serde::Serialize + ?Sized,
{
    let bytes = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let len = bytes.len() as u32;
    writer.write_all(&len.to_ne_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{BufReader, BufWriter};

    fn frame(value: &serde_json::Value) -> Vec<u8> {
        let bytes = serde_json::to_vec(value).unwrap();
        let len = bytes.len() as u32;
        let mut v = len.to_ne_bytes().to_vec();
        v.extend_from_slice(&bytes);
        v
    }

    // 正常：单条消息往返。
    #[tokio::test]
    async fn frame_roundtrip_normal() {
        let data = frame(&serde_json::json!({"type": "ping"}));
        let mut reader = BufReader::new(&data[..]);
        match read_frame(&mut reader).await.unwrap() {
            ReadFrame::Message(p) => {
                let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
                assert_eq!(v["type"], "ping");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    // 长度为 0：空消息（合法 JSON 解析需由调用方处理，这里仅验证能读空负载）。
    #[tokio::test]
    async fn frame_zero_length() {
        let mut data = (0u32).to_ne_bytes().to_vec();
        // 额外写一条正常消息，验证 0 长帧后仍能继续。
        let ok = frame(&serde_json::json!({"type": "ping"}));
        data.extend_from_slice(&ok);
        let mut reader = BufReader::new(&data[..]);
        match read_frame(&mut reader).await.unwrap() {
            ReadFrame::Message(p) => assert!(p.is_empty()),
            other => panic!("expected Message(empty), got {other:?}"),
        }
        match read_frame(&mut reader).await.unwrap() {
            ReadFrame::Message(p) => {
                let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
                assert_eq!(v["type"], "ping");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    // 长度超过上限：不得先分配大缓冲区，应返回 TooLarge。
    #[tokio::test]
    async fn frame_over_limit() {
        let data = (200_000u32).to_ne_bytes().to_vec();
        // 不写入负载，仅长度前缀；read_frame 应在读负载前判定 TooLarge。
        let mut reader = BufReader::new(&data[..]);
        match read_frame(&mut reader).await.unwrap() {
            ReadFrame::TooLarge => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    // 半截数据后输入关闭：返回 Eof。
    #[tokio::test]
    async fn frame_truncated_then_eof() {
        let mut data = (10u32).to_ne_bytes().to_vec();
        data.extend_from_slice(&[1, 2, 3]); // 只 3 字节负载，缺 7 字节
        let mut reader = BufReader::new(&data[..]);
        match read_frame(&mut reader).await.unwrap() {
            ReadFrame::Eof => {}
            other => panic!("expected Eof, got {other:?}"),
        }
    }

    // 连续多条消息。
    #[tokio::test]
    async fn frame_multiple() {
        let mut data = Vec::new();
        for i in 0..5 {
            let f = frame(&serde_json::json!({"i": i}));
            data.extend_from_slice(&f);
        }
        let mut reader = BufReader::new(&data[..]);
        for i in 0..5 {
            match read_frame(&mut reader).await.unwrap() {
                ReadFrame::Message(p) => {
                    let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
                    assert_eq!(v["i"], i);
                }
                other => panic!("expected Message, got {other:?}"),
            }
        }
    }

    /// 序列化失败时 `write_frame` 必须返回 Err 且不写出任何字节（含空帧）。
    struct Unserializable;

    impl serde::Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom(
                "intentional serialization failure",
            ))
        }
    }

    #[tokio::test]
    async fn write_frame_failure_emits_no_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = BufWriter::new(&mut buf);
            let res = write_frame(&mut w, &Unserializable).await;
            assert!(res.is_err(), "序列化失败必须返回 Err");
            w.flush().await.unwrap();
        }
        assert!(buf.is_empty(), "失败时不得发送空帧或任何字节: {buf:?}");
    }

    // write_frame 与 read_frame 端到端。
    #[tokio::test]
    async fn frame_write_then_read() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = BufWriter::new(&mut buf);
            write_frame(
                &mut w,
                &serde_json::json!({"type": "enqueue", "url": "http://x/y"}),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
        }
        let mut reader = BufReader::new(&buf[..]);
        match read_frame(&mut reader).await.unwrap() {
            ReadFrame::Message(p) => {
                let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
                assert_eq!(v["type"], "enqueue");
                assert_eq!(v["url"], "http://x/y");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }
}
