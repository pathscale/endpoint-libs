//! The sans-IO codec in `libs::frame`: round trips, the exact bytes, and the refusals.
#![cfg(feature = "frame-core")]

use endpoint_libs::libs::frame::*;

#[test]
fn text_frame_is_length_kind_payload() {
    let mut out = Vec::new();
    encode_text("hello", &mut out).unwrap();
    assert_eq!(out, [0, 0, 0, 6, 0, b'h', b'e', b'l', b'l', b'o']);
}

#[test]
fn close_frame_forms() {
    let mut out = Vec::new();
    encode_close(None, &mut out).unwrap();
    encode_close(Some((1000, "bye")), &mut out).unwrap();
    assert_eq!(out, [0, 0, 0, 1, 4, 0, 0, 0, 6, 4, 0x03, 0xe8, b'b', b'y', b'e']);
    let mut decoder = FrameDecoder::new();
    decoder.push(&out);
    assert_eq!(decoder.next_frame().unwrap(), Some(Frame::Close(None)));
    assert_eq!(decoder.next_frame().unwrap(), Some(Frame::Close(Some((1000, b"bye".to_vec())))));
    assert_eq!(decoder.next_frame().unwrap(), None);
    decoder.finish().unwrap();
}

#[test]
fn split_across_pushes() {
    let mut out = Vec::new();
    encode_text("ok\nbody", &mut out).unwrap();
    let mut decoder = FrameDecoder::new();
    for byte in &out[..out.len() - 1] {
        decoder.push(&[*byte]);
        assert_eq!(decoder.next_frame().unwrap(), None);
    }
    assert_eq!(decoder.finish(), Err(FrameError::Truncated));
    decoder.push(&out[out.len() - 1..]);
    assert_eq!(decoder.next_frame().unwrap().unwrap().as_text(), Some("ok\nbody"));
}

#[test]
fn refusals() {
    let mut decoder = FrameDecoder::with_max_frame(8);
    decoder.push(&[0, 0, 0, 9]);
    assert_eq!(decoder.next_frame(), Err(FrameError::TooLarge { length: 9, max: 8 }));
    let mut decoder = FrameDecoder::new();
    decoder.push(&[0, 0, 0, 0]);
    assert_eq!(decoder.next_frame(), Err(FrameError::EmptyFrame));
    let mut decoder = FrameDecoder::new();
    decoder.push(&[0, 0, 0, 1, 9]);
    assert_eq!(decoder.next_frame(), Err(FrameError::UnknownKind(9)));
    let mut decoder = FrameDecoder::new();
    decoder.push(&[0, 0, 0, 2, 0, 0xff]);
    assert_eq!(decoder.next_frame(), Err(FrameError::InvalidUtf8));
    let mut decoder = FrameDecoder::new();
    decoder.push(&[0, 0, 0, 2, 4, 1]);
    assert_eq!(decoder.next_frame(), Err(FrameError::MalformedClose));
}

/// The bytes are the ones `framed_json_neutral` puts on the wire.
#[cfg(feature = "framed-transport")]
#[test]
fn matches_the_futures_flavour() {
    use endpoint_libs::libs::ws::WireMessage;
    use endpoint_libs::libs::ws::transport::framed_json_neutral;
    use futures::executor::block_on;
    use futures::io::Cursor;
    use futures::{SinkExt, StreamExt};

    let mut ours = Vec::new();
    encode_text("{\"method\":1}", &mut ours).unwrap();
    encode_frame(KIND_BINARY, &[1, 2, 3], &mut ours).unwrap();
    let mut transport = framed_json_neutral(Cursor::new(ours.clone()));
    let first = block_on(transport.next()).unwrap().unwrap();
    assert_eq!(first.as_text(), Some("{\"method\":1}"));
    let second = block_on(transport.next()).unwrap().unwrap();
    assert!(matches!(second, WireMessage::Binary(ref b) if b.as_ref() == [1, 2, 3]));

}
