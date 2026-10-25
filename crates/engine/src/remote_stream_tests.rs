//! Tests of the playback stream's messages and edit tracking ([`crate::remote_stream`]).

use std::sync::Arc;

use effectcraft_project::build::layer;
use effectcraft_project::{Comp, ItemId, ItemKind, LayerSource, Project};
use effectcraft_time::{FrameRate, Tick};
use serde_json::json;

use crate::color::Label;
use crate::remote::{FrameMsg, FrameReply};
use crate::remote_stream::*;

fn secs(s: f64) -> Tick {
    Tick::from_seconds_f64(s)
}

/// A main comp with three null layers (in at 0, 10 and 20 s; the third parented to the second)
/// and a precomp layer (in at 5 s) showing a comp with one layer.
fn project() -> (Project, ItemId, ItemId) {
    let mut p = Project::default();
    let rate = FrameRate { num: 30000, den: 1001 };
    let mut inner = Comp::new(320, 180, rate, secs(30.0));
    let l = layer(&mut p, &inner, "inner", LayerSource::Null, (100, 100), None);
    inner.layers.push(l);
    let pre = p.add_item("Pre", Label::None, None, ItemKind::Comp(Arc::new(inner)));
    let mut main = Comp::new(1920, 1080, rate, secs(60.0));
    for (i, at) in [0.0, 10.0, 20.0].into_iter().enumerate() {
        let mut l = layer(&mut p, &main, &format!("n{i}"), LayerSource::Null, (100, 100), None);
        l.in_point = secs(at);
        main.layers.push(l);
    }
    let parent = main.layers[1].id;
    main.layers[2].parent = Some(parent);
    let mut pl = layer(&mut p, &main, "pre", LayerSource::Comp { item: pre }, (320, 180), None);
    pl.in_point = secs(5.0);
    main.layers.push(pl);
    let id = p.add_item("Main", Label::None, None, ItemKind::Comp(Arc::new(main)));
    (p, id, pre)
}

#[test]
fn an_edit_counts_from_the_changed_layer_and_what_follows_it() {
    let (old, main, pre) = project();
    // Nothing changed.
    assert_eq!(first_change(&old, &old.clone(), main), None);
    // The last null (in at 20 s): from 20 s.
    let mut new = old.clone();
    new.comp_mut(main).unwrap().layers.get_mut(2).unwrap().name = "renamed".into();
    assert_eq!(first_change(&old, &new, main), Some(secs(20.0)), "conservative: any change to the layer");
    let mut new2 = old.clone();
    let l = new2.comp_mut(main).unwrap().layers.get_mut(2).unwrap();
    l.out_point = secs(50.0);
    assert_eq!(first_change(&old, &new2, main), Some(secs(20.0)));
    // Its parent (in at 10 s) changes it too: from 10 s.
    let mut new3 = old.clone();
    new3.comp_mut(main).unwrap().layers.get_mut(1).unwrap().out_point = secs(40.0);
    assert_eq!(first_change(&old, &new3, main), Some(secs(10.0)));
    // Inside the precomp: the precomp layer counts whole (in at 5 s).
    let mut new4 = old.clone();
    new4.comp_mut(pre).unwrap().layers.get_mut(0).unwrap().out_point = secs(3.0);
    assert_eq!(first_change(&old, &new4, main), Some(secs(5.0)));
    // A deleted layer counts from its in-point; an added one from its own.
    let mut new5 = old.clone();
    new5.comp_mut(main).unwrap().layers.remove(1);
    assert_eq!(first_change(&old, &new5, main), Some(secs(10.0)));
    // Project settings: from the start.
    let mut new6 = old.clone();
    new6.settings.gpu_acceleration = !new6.settings.gpu_acceleration;
    assert_eq!(first_change(&old, &new6, main), Some(Tick(i64::MIN)));
    // Another comp that the main comp doesn't show: nothing.
    let mut new7 = old.clone();
    let other = new7.add_item("Other", Label::None, None, ItemKind::Comp(Arc::new(Comp::new(10, 10, FrameRate::default(), secs(1.0)))));
    assert!(new7.comp(other).is_some());
    assert_eq!(first_change(&old, &new7, main), None);
}

#[test]
fn the_chunk_grid_covers_the_range_exactly() {
    // Frames [20, 50) on a grid anchored at 23, in chunks of 15: 20–22 (index −1, clipped), 23–37,
    // 38–49 (clipped at the end).
    assert_eq!(chunk_frames(-1, 15, 23, 20, 50), Some((20, 3)));
    assert_eq!(chunk_frames(0, 15, 23, 20, 50), Some((23, 15)));
    assert_eq!(chunk_frames(1, 15, 23, 20, 50), Some((38, 12)));
    assert_eq!(chunk_frames(2, 15, 23, 20, 50), None);
    assert_eq!(chunk_frames(-2, 15, 23, 20, 50), None);
    assert_eq!((chunk_of(22, 15, 23), chunk_of(23, 15, 23), chunk_of(37, 15, 23), chunk_of(38, 15, 23)), (-1, 0, 0, 1));
    // Absurd input: no overflow, no panic.
    assert_eq!(chunk_frames(i64::MAX, 0, i64::MAX, 0, i64::MAX), None);
    assert_eq!(chunk_frames(i64::MIN, 15, 0, i64::MIN, 0), None);
    let _ = chunk_of(i64::MIN, 0, i64::MAX);
}

#[test]
fn stream_messages_keep_their_shape_and_defaults() {
    let start = json!({"type": "stream", "op": "start", "stream": 1, "revision": 3, "comp": 7, "from": 0, "opts": {}});
    let m: FrameMsg = serde_json::from_value(start).unwrap();
    let FrameMsg::Stream(StreamMsg::Start { chunk_frames, ahead_secs, adaptive, codec, crf, looping, to, .. }) = m else { panic!("{m:?}") };
    assert_eq!(
        (chunk_frames, ahead_secs, adaptive, codec, crf, looping, to),
        (DEFAULT_CHUNK_FRAMES, DEFAULT_AHEAD_SECS, true, StreamCodec::H264, 23.0, false, None)
    );
    let reply = FrameReply::Stream(StreamReply::Chunk {
        stream: 1,
        seq: 2,
        revision: 3,
        index: -4,
        first: 60,
        start: FrameRate { num: 30000, den: 1001 }.tick_of(60),
        frames: 15,
        frame_rate: FrameRate { num: 30000, den: 1001 },
        width: 960,
        height: 540,
        scale: 0.5,
        codec: StreamCodec::H264,
        render_ms: 1.0,
        encode_ms: 2.0,
    });
    let v = serde_json::to_value(&reply).unwrap();
    assert_eq!(
        (v["type"].as_str(), v["op"].as_str(), v["frameRate"]["num"].as_i64(), v["codec"].as_str()),
        (Some("stream"), Some("chunk"), Some(30000), Some("h264"))
    );
    assert_eq!(serde_json::from_value::<FrameReply>(v).unwrap(), reply);
    let ph: StreamMsg = serde_json::from_value(json!({"op": "playhead", "stream": 1, "time": 5})).unwrap();
    assert!(matches!(ph, StreamMsg::Playhead { playing: true, received: None, .. }));
    // The byte codec of the wire carries them too.
    let m = FrameMsg::Stream(StreamMsg::Seek { stream: 1, time: secs(2.0) });
    assert!(matches!(crate::remote::decode_msg(&crate::remote::encode_msg(&m)).unwrap(), FrameMsg::Stream(StreamMsg::Seek { stream: 1, .. })));
}

/// Put `text` as the expression of layer `layer`'s opacity.
fn with_expression(p: &mut Project, comp: ItemId, layer: usize, text: &str) {
    let l = p.comp_mut(comp).unwrap().layers.get_mut(layer).unwrap();
    l.props.prop_mut("transform/opacity").unwrap().expr = Some(effectcraft_project::props::Expression { text: text.into(), enabled: true });
}

/// Edit audit B7, 18 Oct: a layer whose expression reads what an edit changed changes with it.
/// Before, only the edited layer counted, so frames of the reading layer before the edited one's
/// in-point kept their old pixels (and, on the Mac, were kept as current).
#[test]
fn an_expression_that_reads_the_edited_layer_or_comp_counts_from_its_own_in_point() {
    let (base, main, _) = project();
    // A controls comp the main comp doesn't show.
    let mut base = base;
    let mut controls = Comp::new(10, 10, FrameRate::default(), secs(60.0));
    let ctrl = layer(&mut base, &controls, "CTRL", LayerSource::Null, (10, 10), None);
    controls.layers.push(ctrl);
    let controls = base.add_item("Controls", Label::None, None, ItemKind::Comp(Arc::new(controls)));

    // n0 (in at 0) reads n2 (in at 20) by name: editing n2 changes n0's frames from 0.
    let mut old = base.clone();
    with_expression(&mut old, main, 0, "thisComp.layer(\"n2\").transform.opacity");
    let mut new = old.clone();
    new.comp_mut(main).unwrap().layers.get_mut(2).unwrap().out_point = secs(50.0);
    assert_eq!(first_change(&old, &new, main), Some(secs(0.0)));

    // n0 reads a layer of the controls comp: editing it changes n0 (and nothing else) from 0.
    let mut old = base.clone();
    with_expression(&mut old, main, 0, "comp(\"Controls\").layer(\"CTRL\").transform.opacity");
    let mut new = old.clone();
    new.comp_mut(controls).unwrap().layers.get_mut(0).unwrap().out_point = secs(30.0);
    assert_eq!(first_change(&old, &new, main), Some(secs(0.0)));
    // ... while without the expression the controls comp is nothing the main comp shows.
    let mut plain = base.clone();
    plain.comp_mut(controls).unwrap().layers.get_mut(0).unwrap().out_point = secs(30.0);
    assert_eq!(first_change(&base, &plain, main), None);

    // An expression reading only its own layer (wiggle) doesn't widen an edit elsewhere.
    let mut old = base.clone();
    with_expression(&mut old, main, 0, "wiggle(2, 10)");
    let mut new = old.clone();
    new.comp_mut(main).unwrap().layers.get_mut(2).unwrap().out_point = secs(50.0);
    assert_eq!(first_change(&old, &new, main), Some(secs(20.0)));
    // ... nor does an edit of an unrelated comp.
    let mut new = old.clone();
    new.comp_mut(controls).unwrap().layers.get_mut(0).unwrap().out_point = secs(30.0);
    assert_eq!(first_change(&old, &new, main), None);

    // A lookup built at run time can read anything: any edit counts it.
    let mut old = base.clone();
    with_expression(&mut old, main, 1, "var n = \"Con\" + \"trols\"; comp(n).layer(1).transform.opacity");
    let mut new = old.clone();
    new.comp_mut(controls).unwrap().layers.get_mut(0).unwrap().out_point = secs(30.0);
    assert_eq!(first_change(&old, &new, main), Some(secs(10.0)), "n1 (in at 10) and its child n2");

    // Renaming the comp an expression names counts (the name now finds another item, or none).
    let mut old = base.clone();
    with_expression(&mut old, main, 0, "comp(\"Controls\").layer(1).transform.opacity");
    let mut new = old.clone();
    if let Some(item) = new.items.get_mut(&controls) {
        item.name = "Controls old".into();
    }
    assert_eq!(first_change(&old, &new, main), Some(secs(0.0)));
}
