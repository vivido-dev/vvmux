//! Regressions for cursor uploads, cropping, and pane-local replies.

use super::*;

fn send(graphics: &mut CursorGraphics, body: &str) -> GraphicsOutput {
    graphics.process(format!("\x1b_G{body}\x1b\\").as_bytes(), true, (8, 16))
}

fn upload(output: &GraphicsOutput) -> (&[u8], KittyGraphicsReply) {
    output
        .commands
        .iter()
        .find_map(|command| match command {
            KittyGraphicsCommand::Packet {
                bytes,
                reply: Some(reply),
                ..
            } => Some((bytes.as_slice(), *reply)),
            _ => None,
        })
        .expect("upload and acknowledgement")
}

fn pixels(bytes: &[u8]) -> Vec<u8> {
    let mut pixels = Vec::new();
    for packet in bytes
        .split(|b| *b == 0x1b)
        .filter(|packet| packet.starts_with(b"_G"))
    {
        let separator = packet.iter().position(|b| *b == b';').expect("separator");
        pixels.extend(
            base64::engine::general_purpose::STANDARD
                .decode(&packet[separator + 1..])
                .expect("base64"),
        );
    }
    pixels
}

#[test]
fn standard_cursor_upload_crops_pixels_and_echoes_application_ids() {
    let mut graphics = CursorGraphics::default();
    let data = base64::engine::general_purpose::STANDARD
        .encode([255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
    let output = send(
        &mut graphics,
        &format!("a=T,i=1,p=7,f=24,s=2,v=2,x=1,y=0,w=1,h=2,c=1,r=2,C=1,m=0;{data}"),
    );
    let (bytes, reply) = upload(&output);
    assert_eq!(pixels(bytes), [0, 255, 0, 255, 255, 255]);
    assert_eq!(reply.encode(None).unwrap(), b"\x1b_Gi=1,p=7;OK\x1b\\");
    assert!(output.placement.as_ref().unwrap().keep_cursor);
    assert!(
        std::str::from_utf8(bytes)
            .unwrap()
            .contains("s=1,v=2,c=1,r=2,U=1")
    );
    assert!(!std::str::from_utf8(bytes).unwrap().contains(",x="));
    let output = send(&mut graphics, "a=p,i=1,p=7,x=0,y=1,w=2,h=1,c=2,r=1,C=1");
    assert_eq!(pixels(upload(&output).0), [0, 0, 255, 255, 255, 255]);
    assert_eq!(output.clear.len(), 1);
}

#[test]
fn chunked_upload_finishes_once_and_accepts_unpadded_final_base64() {
    let mut graphics = CursorGraphics::default();
    let first = send(&mut graphics, "a=T,i=3,f=32,s=1,v=1,c=1,r=1,m=1;AAAA");
    assert!(first.commands.is_empty());
    assert!(first.placement.is_none());
    let last = send(&mut graphics, "m=0;AA");
    assert_eq!(pixels(upload(&last).0), [0, 0, 0, 0]);
    assert_eq!(upload(&last).1.image_id, 3);
    assert!(graphics.pending.is_none());
}

#[test]
fn bad_chunks_and_unsupported_features_release_pending_pixels() {
    let mut graphics = CursorGraphics::default();
    send(&mut graphics, "a=T,i=3,f=32,s=1,v=1,c=1,r=1,m=1;AAAA");
    let output = send(&mut graphics, "m=0;AAAA");
    assert!(matches!(
        output.commands.as_slice(),
        [KittyGraphicsCommand::Rejected {
            image_id: 3,
            error: "EINVAL:too many image bytes",
            ..
        }]
    ));
    assert!(graphics.images.is_empty());
    assert!(graphics.pending.is_none());
    for body in [
        "a=T,i=4,t=s,f=32,s=1,v=1;L25hbWU=",
        "a=T,i=4,f=32,s=4294967295,v=2;AAAA",
        "a=T,i=4,f=32,s=1,v=1,c=4294967295,r=1;AAAAAA==",
        "a=p,i=4,P=2,Q=3",
        "a=f,i=4",
    ] {
        let output = send(&mut graphics, body);
        assert!(
            matches!(
                output.commands.as_slice(),
                [KittyGraphicsCommand::Rejected { .. }]
            ),
            "{body}"
        );
        assert!(output.placement.is_none());
    }
    assert!(graphics.images.is_empty());
}

#[test]
fn independent_panes_reusing_local_ids_cannot_delete_each_others_cursor_images() {
    let mut left = CursorGraphics::default();
    let mut right = CursorGraphics::default();
    let a = send(&mut left, "a=T,i=1,f=24,s=1,v=1,c=1,r=1;AAAA");
    let b = send(&mut right, "a=T,i=1,f=24,s=1,v=1,c=1,r=1;/wAA");
    let left_id = a.placement.unwrap().host_id;
    let right_id = b.placement.unwrap().host_id;
    assert_ne!(left_id, right_id);
    let deleted = send(&mut left, "a=d,d=R,x=1,y=4294967295");
    assert_eq!(deleted.clear, [left_id]);
    assert!(left.images.is_empty());
    assert_eq!(right.images[&1].pixels.data, [255, 0, 0]);
    let placed = send(&mut right, "a=p,i=1,p=5,c=1,r=1");
    assert_eq!(pixels(upload(&placed).0), [255, 0, 0]);
    assert!(placed.clear.is_empty());
    assert_eq!(send(&mut right, "a=d").clear.len(), 2);
    assert_eq!(right.images.len(), 1, "soft delete retains source pixels");
    assert_eq!(send(&mut right, "a=d,d=I,i=1").clear.len(), 0);
    assert!(right.images.is_empty());
}

#[test]
fn invalid_replacement_preserves_previous_image_and_placement() {
    let mut graphics = CursorGraphics::default();
    let good = send(&mut graphics, "a=T,i=1,f=24,s=1,v=1,c=1,r=1;/wAA");
    let host_id = good.placement.unwrap().host_id;
    let bad = send(&mut graphics, "a=T,i=1,f=24,s=1,v=1,x=2,c=1,r=1;AAAA");
    assert!(bad.placement.is_none());
    assert_eq!(graphics.images[&1].pixels.data, [255, 0, 0]);
    assert_eq!(graphics.images[&1].placements, [(0, host_id)]);
}
