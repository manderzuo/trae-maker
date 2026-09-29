//! Verified local media metadata for reference-video risk profiles.
pub(super) fn owned_reference_seconds(root:&std::path::Path,owner:&str,body:&serde_json::Value)->Result<Option<u64>,String> {
    if body["video_urls"].as_array().is_some_and(|v|!v.is_empty()) {
        return Err("reference_video_budget_metadata_required".into());
    }
    let mut total=0u64;
    for (field,mime) in [("image_asset_ids","image/"),("video_asset_ids","video/")] {
        for id in body[field].as_array().into_iter().flatten() {
            let (record,bytes)=super::assets::read_owned(root,owner,id.as_str().ok_or(INVALID)?)
                .map_err(|_|"reference_asset_unavailable")?;
            if !record.mime_type.starts_with(mime) {return Err("reference_asset_type_mismatch".into());}
            if field=="video_asset_ids" {
                if record.mime_type!="video/mp4" {return Err("reference_video_format_unsupported".into());}
                let report=timeline(&bytes)?;
                let ms=if report.deep {
                    super::reference_timeline::verify(root,&bytes,&report)?
                } else {report.ms};
                total=total.checked_add(ms).ok_or(INVALID)?;
            }
        }
    }
    if total>60_000 {return Err(INVALID.into());}
    Ok(if total==0 {None} else {Some((total+999)/1000)})
}
const INVALID: &str = "reference_video_metadata_invalid";
fn u32_at(b: &[u8], n: usize) -> Result<u32, String> {
    Ok(u32::from_be_bytes(b.get(n..n+4).ok_or(INVALID)?.try_into().map_err(|_|INVALID)?))
}
fn u64_at(b: &[u8], n: usize) -> Result<u64, String> {
    Ok(u64::from_be_bytes(b.get(n..n+8).ok_or(INVALID)?.try_into().map_err(|_|INVALID)?))
}
fn boxes(b: &[u8]) -> Result<Vec<(&[u8], &[u8])>, String> {
    let mut at=0usize; let mut out=Vec::new();
    while at<b.len() {
        if out.len()>=10_000 {return Err(INVALID.into());}
        let short=u32_at(b,at)?;
        let (size,header)=match short {0=>(b.len()-at,8),1=>(usize::try_from(u64_at(b,at+8)?).map_err(|_|INVALID)?,16),n=>(n as usize,8)};
        let end=at.checked_add(size).filter(|n|*n<=b.len()).ok_or(INVALID)?;
        if size<header {return Err(INVALID.into());}
        out.push((b.get(at+4..at+8).ok_or(INVALID)?,b.get(at+header..end).ok_or(INVALID)?));
        at=end;
    }
    Ok(out)
}
fn one<'a>(items:&[(&[u8], &'a [u8])], kind:&[u8;4]) -> Result<&'a [u8],String> {
    let mut values=items.iter().filter(|(k,_)|*k==kind);
    let found=values.next().ok_or(INVALID)?.1;
    if values.next().is_some() {return Err(INVALID.into());} Ok(found)
}
fn media_time(mdhd:&[u8])->Result<(u64,u64),String> {
    let (offset,wide)=match mdhd.first() {Some(0)=>(12,false),Some(1)=>(20,true),_=>return Err(INVALID.into())};
    let scale=u64::from(u32_at(mdhd,offset)?);
    let ticks=if wide {u64_at(mdhd,offset+4)?} else {u64::from(u32_at(mdhd,offset+4)?)};
    if scale==0 || ticks==0 {return Err(INVALID.into());} Ok((scale,ticks))
}
fn edit_duration_ms(edts:&[u8],movie_scale:u64,media_ticks:u64)->Result<u64,String> {
    let edit=boxes(edts)?;let list=one(&edit,b"elst")?;
    let wide=match list.first() {Some(0)=>false,Some(1)=>true,_=>return Err(INVALID.into())};
    if list.get(1..4)!=Some(&[0,0,0]) {return Err(INVALID.into());}
    let count=u32_at(list,4)? as usize;let width=if wide {20} else {12};
    if count==0 || count>10000 || list.len()!=8+count*width {return Err(INVALID.into());}
    let mut ticks=0u64;
    for index in 0..count {
        let offset=8+index*width;
        let (duration,start,rate)=if wide {(u64_at(list,offset)?,u64_at(list,offset+8)? as i64,offset+16)}
            else {(u32_at(list,offset)? as u64,u32_at(list,offset+4)? as i32 as i64,offset+8)};
        // Empty edits use -1. Offset+duration may exceed mdhd due to video
        // composition timestamps; budget the larger complete timeline instead
        // of incorrectly rejecting ordinary B-frame output.
        if duration==0 || start< -1 || start>=0 && start as u64>media_ticks || u32_at(list,rate)?!=0x00010000 {return Err(INVALID.into());}
        ticks=ticks.checked_add(duration).ok_or(INVALID)?;
    }
    let ms=ticks.checked_mul(1000).and_then(|n|n.checked_add(movie_scale-1)).ok_or(INVALID)?/movie_scale;
    if ms==0 || ms>60_000 {return Err(INVALID.into());}Ok(ms)
}
#[derive(Debug)]
pub(super) struct Timeline {
    pub ms: u64,
    pub deep: bool,
    pub video_scale: u64,
    pub video_ticks: u64,
    pub video_samples: u64,
}
#[cfg(test)]
pub(super) fn duration_ms(bytes: &[u8]) -> Result<u64, String> {
    let report=timeline(bytes)?;
    if report.deep {return Err(INVALID.into());} Ok(report.ms)
}
fn milliseconds(ticks:u64,scale:u64)->Result<u64,String> {
    ticks.checked_mul(1000).and_then(|v|v.checked_add(scale-1)).map(|v|v/scale).ok_or_else(||INVALID.into())
}
// For discrepant headers, independently check that every timed sample exists
// inside this file's mdat. No client duration, remote URL or external data
// reference is evidence of a local media timeline.
fn sample_extent(table:&[(&[u8],&[u8])],bytes:&[u8],mdat:&[u8],samples:u64)->Result<(),String> {
    let sizes=one(table,b"stsz")?;
    if sizes.get(..4)!=Some(&[0,0,0,0]) || u64::from(u32_at(sizes,8)?)!=samples {return Err(INVALID.into());}
    let uniform=u32_at(sizes,4)? as u64;
    if sizes.len()!=if uniform==0 {12+samples as usize*4} else {12} {return Err(INVALID.into());}
    let map=one(table,b"stsc")?;
    let count=u32_at(map,4)? as usize;
    if map.get(..4)!=Some(&[0,0,0,0])||count==0||count>10000||map.len()!=8+count*12 {return Err(INVALID.into());}
    let mut runs=Vec::new();
    for n in 0..count {
        let first=u32_at(map,8+n*12)? as usize;
        let size=u32_at(map,12+n*12)? as usize;
        if size==0||first==0||n==0&&first!=1||runs.last().is_some_and(|(prev,_)|*prev>=first)||u32_at(map,16+n*12)?!=1 {return Err(INVALID.into());}
        runs.push((first,size));
    }
    let description=one(table,b"stsd")?;
    if description.get(..4)!=Some(&[0,0,0,0])||u32_at(description,4)?!=1 {return Err(INVALID.into());}
    let entries=boxes(description.get(8..).ok_or(INVALID)?)?;
    if entries.len()!=1||entries[0].1.get(6..8)!=Some(&[0,1]) {return Err(INVALID.into());}
    let offsets:Vec<_>=table.iter().filter(|(k,_)|*k==b"stco"||*k==b"co64").collect();
    if offsets.len()!=1 {return Err(INVALID.into());}
    let (kind,chunks)=*offsets[0];let wide=kind==b"co64";
    let count=u32_at(chunks,4)? as usize;
    if chunks.get(..4)!=Some(&[0,0,0,0])||count==0||count>10000||chunks.len()!=8+count*if wide {8} else {4}||runs.last().unwrap().0>count {return Err(INVALID.into());}
    let start=(mdat.as_ptr() as usize).checked_sub(bytes.as_ptr() as usize).ok_or(INVALID)? as u64;
    let end=start.checked_add(mdat.len() as u64).ok_or(INVALID)?;
    let mut sample=0usize;let mut run=0usize;let mut ranges=Vec::new();
    for chunk in 1..=count {
        if run+1<runs.len()&&runs[run+1].0==chunk {run+=1;}
        let at=if wide {u64_at(chunks,8+(chunk-1)*8)?} else {u32_at(chunks,8+(chunk-1)*4)? as u64};
        let mut length=0u64;
        for _ in 0..runs[run].1 {
            if sample>=samples as usize {return Err(INVALID.into());}
            let size=if uniform>0 {uniform} else {u32_at(sizes,12+sample*4)? as u64};
            if size==0 {return Err(INVALID.into());}
            length=length.checked_add(size).ok_or(INVALID)?;sample+=1;
        }
        let tail=at.checked_add(length).ok_or(INVALID)?;
        if at<start||tail>end {return Err(INVALID.into());}ranges.push((at,tail));
    }
    if sample!=samples as usize {return Err(INVALID.into());}
    ranges.sort_unstable();
    if ranges.windows(2).any(|r|r[0].1>r[1].0) {return Err(INVALID.into());}Ok(())
}
fn composition_ms(table:&[(&[u8],&[u8])],durations:&[u64],scale:u64)->Result<u64,String> {
    let lists:Vec<_>=table.iter().filter(|(k,_)|*k==b"ctts").collect();
    if lists.is_empty() {return Ok(0);}if lists.len()!=1 {return Err(INVALID.into());}
    let list=lists[0].1;
    if !matches!(list.first(),Some(0|1))||list.get(1..4)!=Some(&[0,0,0]) {return Err(INVALID.into());}
    let count=u32_at(list,4)? as usize;
    if count==0||count>10000||list.len()!=8+count*8 {return Err(INVALID.into());}
    let mut at=0usize;let mut decode=0i64;let mut minimum=i64::MAX;let mut maximum=i64::MIN;
    for n in 0..count {
        let count=u32_at(list,8+n*8)? as usize;
        let raw=u32_at(list,12+n*8)?;
        let offset=if list[0]==1 {raw as i32 as i64} else {raw as i64};
        if count==0||count>durations.len().saturating_sub(at)||offset.unsigned_abs()>scale.checked_mul(60).ok_or(INVALID)? {return Err(INVALID.into());}
        for _ in 0..count {
            let start=decode.checked_add(offset).ok_or(INVALID)?;
            let duration=i64::try_from(durations[at]).map_err(|_|INVALID)?;
            minimum=minimum.min(start);maximum=maximum.max(start.checked_add(duration).ok_or(INVALID)?);
            decode=decode.checked_add(duration).ok_or(INVALID)?;at+=1;
        }
    }
    if at!=durations.len() {return Err(INVALID.into());}
    milliseconds(u64::try_from(maximum.checked_sub(minimum).ok_or(INVALID)?).map_err(|_|INVALID)?,scale)
}
fn timeline(bytes: &[u8]) -> Result<Timeline, String> {
    if bytes.len()>super::assets::MAX_ASSET_BYTES {return Err(INVALID.into());}
    let top=boxes(bytes)?;
    one(&top,b"ftyp")?; one(&top,b"mdat")?;
    if top.iter().any(|(k,_)|*k==b"moof") {return Err(INVALID.into());}
    let movie=boxes(one(&top,b"moov")?)?;
    if movie.iter().any(|(k,_)|*k==b"mvex") {return Err(INVALID.into());}
    let mut video_count=0;let mut total=0;
    let mut report=Timeline {ms:0,deep:false,video_scale:0,video_ticks:0,video_samples:0};
    for (_,track) in movie.iter().filter(|(k,_)|*k==b"trak") {
        let track=boxes(track)?;
        let media=boxes(one(&track,b"mdia")?)?;
        let handler=one(&media,b"hdlr")?.get(8..12).ok_or(INVALID)?;
        if handler==b"vide" {video_count+=1;} else if handler!=b"soun" {return Err(INVALID.into());}
        let (scale,ticks)=media_time(one(&media,b"mdhd")?)?;
        let info=boxes(one(&media,b"minf")?)?;
        let table=boxes(one(&info,b"stbl")?)?;
        let timing=one(&table,b"stts")?;
        if timing.get(..4)!=Some(&[0,0,0,0]) {return Err(INVALID.into());}
        let entries=u32_at(timing,4)? as usize;
        if entries==0 || entries>10000 || timing.len()!=8+entries*8 {return Err(INVALID.into());}
        let mut measured=0u64;let mut samples=0u64;let mut max_delta=0u64;
        for n in 0..entries {
            let count=u64::from(u32_at(timing,8+n*8)?);let delta=u64::from(u32_at(timing,12+n*8)?);
            if count==0 || delta==0 {return Err(INVALID.into());}
            samples=samples.checked_add(count).ok_or(INVALID)?;
            measured=measured.checked_add(count.checked_mul(delta).ok_or(INVALID)?).ok_or(INVALID)?;
            max_delta=max_delta.max(delta);
        }
        // Audio priming/padding can make stts longer than mdhd when an edit
        // list trims playback. Never reduce the reservation to the trimmed
        // duration; retain the whole decoded sample timeline.
        let edited_audio_padding=handler==b"soun" && measured>=ticks
            && track.iter().any(|(k,_)|*k==b"edts");
        if samples>1_000_000 {return Err(INVALID.into());}
        let mut ms=milliseconds(ticks.max(measured),scale)?;
        if measured!=ticks && !edited_audio_padding {
            if handler!=b"vide"||samples>10000 {return Err(INVALID.into());}
            sample_extent(&table,bytes,one(&top,b"mdat")?,samples)?;
            let mut durations=Vec::with_capacity(samples as usize);
            for n in 0..entries {
                durations.extend(std::iter::repeat(u32_at(timing,12+n*8)? as u64).take(u32_at(timing,8+n*8)? as usize));
            }
            ms=ms.max(composition_ms(&table,&durations,scale)?);
            // One frame is a fast-path threshold, never a rejection threshold.
            report.deep=ticks.abs_diff(measured)>max_delta;
        }
        if handler==b"vide" {report.video_scale=scale;report.video_ticks=measured;report.video_samples=samples;}
        if track.iter().any(|(k,_)|*k==b"edts") {
            let (movie_scale,_)=media_time(one(&movie,b"mvhd")?)?;
            ms=ms.max(edit_duration_ms(one(&track,b"edts")?,movie_scale,ticks)?);
        }
        if ms==0 || ms>60_000 {return Err(INVALID.into());} total=total.max(ms);
    }
    if video_count!=1 {return Err(INVALID.into());}report.ms=total; Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bx(kind: &[u8;4], data: Vec<u8>) -> Vec<u8> {
        let mut b=((data.len()+8) as u32).to_be_bytes().to_vec();
        b.extend(kind); b.extend(data); b
    }
    fn sample(ticks:u32, count:u32, delta:u32) -> Vec<u8> {
        let mut mdhd=vec![0;12]; mdhd.extend(24000u32.to_be_bytes()); mdhd.extend(ticks.to_be_bytes()); mdhd.extend([0;4]);
        let mut hdlr=vec![0;8]; hdlr.extend(b"vide"); hdlr.extend([0;12]);
        let mut stts=vec![0;4]; stts.extend(1u32.to_be_bytes()); stts.extend(count.to_be_bytes()); stts.extend(delta.to_be_bytes());
        let mdia=[bx(b"mdhd",mdhd),bx(b"hdlr",hdlr),bx(b"minf",bx(b"stbl",bx(b"stts",stts)))].concat();
        [bx(b"ftyp",b"isom\0\0\0\0isom".to_vec()),bx(b"moov",bx(b"trak",bx(b"mdia",mdia))),bx(b"mdat",vec![0;4])].concat()
    }
    #[test]
    fn reference_duration_uses_sample_timing_and_rounds_up_not_client_declaration() {
        assert_eq!(duration_ms(&sample(121000,121,1000)).unwrap(),5042);
        assert_eq!(duration_ms(&sample(120000,120,1000)).unwrap(),5000);
        assert!(duration_ms(&sample(24000,121,1000)).is_err(),"short forged mdhd must not underbudget a longer sample table");
        assert!(duration_ms(&sample(0,0,0)).is_err());
        assert!(duration_ms(&sample(2400000,2400,1000)).is_err(),"bound reference duration");
        let valid=sample(120000,120,1000);
        for len in [0,4,7,valid.len()-1] {assert!(duration_ms(&valid[..len]).is_err());}
        let mut fragmented=valid.clone();fragmented.extend(bx(b"moof",vec![]));assert!(duration_ms(&fragmented).is_err());
        let mut duplicate=valid.clone();duplicate.extend(bx(b"moov",vec![]));assert!(duration_ms(&duplicate).is_err());
    }
    #[test]
    fn owned_references_bind_type_duration_and_owner_without_trusting_remote_uri() {
        use serde_json::json;
        let root=std::env::temp_dir().join(format!("aiwork-ref-{:032x}",rand::random::<u128>()));
        let video=super::super::assets::create(&root,"owner","sample.mp4",Some("video/mp4"),&sample(121000,121,1000)).unwrap();
        let body=json!({"video_asset_ids":[video.id]});
        assert_eq!(owned_reference_seconds(&root,"owner",&body).unwrap(),Some(6));
        assert!(owned_reference_seconds(&root,"another-key",&body).is_err());
        assert!(owned_reference_seconds(&root,"owner",&json!({"image_asset_ids":[video.id]})).is_err());
        assert!(owned_reference_seconds(&root,"owner",&json!({"video_urls":["tos://unknown"]})).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn edited_sample(segment_ms:u32,rate:u16)->Vec<u8> {
        let source=sample(121000,121,1000);let top=boxes(&source).unwrap();
        let movie=boxes(one(&top,b"moov").unwrap()).unwrap();
        let track=one(&movie,b"trak").unwrap();
        let mut mvhd=vec![0;12];mvhd.extend(1000u32.to_be_bytes());mvhd.extend(segment_ms.to_be_bytes());
        let mut elst=vec![0;4];elst.extend(1u32.to_be_bytes());elst.extend(segment_ms.to_be_bytes());elst.extend(1024u32.to_be_bytes());elst.extend(rate.to_be_bytes());elst.extend(0u16.to_be_bytes());
        [bx(b"ftyp",one(&top,b"ftyp").unwrap().to_vec()),bx(b"moov",[bx(b"mvhd",mvhd),bx(b"trak",[bx(b"edts",bx(b"elst",elst)),track.to_vec()].concat())].concat()),bx(b"mdat",vec![0;4])].concat()
    }
    #[test]
    fn normal_mp4_edit_list_uses_conservative_decode_and_presentation_duration() {
        assert_eq!(duration_ms(&edited_sample(5000,1)).unwrap(),5042);
        assert_eq!(duration_ms(&edited_sample(6000,1)).unwrap(),6000);
        assert!(duration_ms(&edited_sample(5000,2)).is_err(),"unverified retiming is not a supported budget profile");
        assert!(duration_ms(&edited_sample(61000,1)).is_err());
    }

    #[test]
    fn edited_audio_priming_is_budgeted_instead_of_rejecting_generated_mp4() {
        let source=edited_sample(5000,1);let top=boxes(&source).unwrap();
        let movie=one(&top,b"moov").unwrap();
        let mut mdhd=vec![0;12];mdhd.extend(32000u32.to_be_bytes());mdhd.extend(162816u32.to_be_bytes());
        let mut hdlr=vec![0;8];hdlr.extend(b"soun");
        let mut timing=vec![0;4];timing.extend(1u32.to_be_bytes());timing.extend(160u32.to_be_bytes());timing.extend(1024u32.to_be_bytes());
        let mdia=bx(b"mdia",[bx(b"mdhd",mdhd),bx(b"hdlr",hdlr),bx(b"minf",bx(b"stbl",bx(b"stts",timing)))].concat());
        let mut edit=vec![0;4];edit.extend(1u32.to_be_bytes());edit.extend(5088u32.to_be_bytes());edit.extend(1024u32.to_be_bytes());edit.extend(0x10000u32.to_be_bytes());
        let assemble=|audio:Vec<u8>| [bx(b"ftyp",one(&top,b"ftyp").unwrap().to_vec()),bx(b"moov",[movie.to_vec(),bx(b"trak",audio)].concat()),bx(b"mdat",vec![0;4])].concat();
        assert_eq!(duration_ms(&assemble([bx(b"edts",bx(b"elst",edit)),mdia.clone()].concat())).unwrap(),5120);
        assert!(duration_ms(&assemble(mdia)).is_err(),"unexplained timing mismatch must remain rejected");
    }

    // Real muxed frames, not a metadata-only mock. The original media must
    // survive verification unchanged and larger discrepancies are not an
    // automatic rejection threshold.
    fn change_video_header(bytes: &mut [u8], ticks: u32) {
        let at = bytes.windows(4).position(|b| b == b"mdhd").unwrap() + 4;
        bytes[at + 16..at + 20].copy_from_slice(&ticks.to_be_bytes());
    }
    #[test]
    fn one_frame_header_difference_is_compatible_without_a_decoder() {
        let f = super::super::video_frames::tests::Fixture::new();
        let mut bytes = std::fs::read(f.clip()).unwrap();
        change_video_header(&mut bytes, 18432); // 8 frames/16384Hz = 1s; header = 1.125s.
        std::fs::remove_file(f.0.join("video-frame-extractor.json")).unwrap();
        let asset = super::super::assets::create(&f.0,"owner","ref.mp4",Some("video/mp4"),&bytes).unwrap();
        assert_eq!(owned_reference_seconds(&f.0,"owner",&serde_json::json!({"video_asset_ids":[asset.id]})).unwrap(),Some(2));
        assert_eq!(super::super::assets::read_owned(&f.0,"owner",&asset.id).unwrap().1,bytes);
    }
    #[test]
    fn multi_frame_difference_uses_real_decoding_and_conservative_duration() {
        let f = super::super::video_frames::tests::Fixture::new();
        for (ticks, seconds) in [(24576,2),(8192,1)] {
            let mut bytes = std::fs::read(f.clip()).unwrap();
            change_video_header(&mut bytes,ticks);
            let asset = super::super::assets::create(&f.0,"owner","ref.mp4",Some("video/mp4"),&bytes).unwrap();
            let body=serde_json::json!({"video_asset_ids":[asset.id]});
            assert_eq!(owned_reference_seconds(&f.0,"owner",&body).unwrap(),Some(seconds));
            assert_eq!(super::super::assets::read_owned(&f.0,"owner",&asset.id).unwrap().1,bytes);
            assert_eq!(owned_reference_seconds(&f.0,"other-key",&body).unwrap_err(),"reference_asset_unavailable");
        }
    }
    #[test]
    fn deep_verification_rejects_undecodable_samples_and_cleans_temporary_media() {
        let f = super::super::video_frames::tests::Fixture::new();
        let mut bytes=std::fs::read(f.clip()).unwrap();
        change_video_header(&mut bytes,24576);
        let at=bytes.windows(4).position(|b|b==b"mdat").unwrap()+4;
        let size=u32::from_be_bytes(bytes[at-8..at-4].try_into().unwrap()) as usize;
        bytes[at..at+size-8].fill(0);
        let asset=super::super::assets::create(&f.0,"owner","broken.mp4",Some("video/mp4"),&bytes).unwrap();
        assert_eq!(owned_reference_seconds(&f.0,"owner",&serde_json::json!({"video_asset_ids":[asset.id]})).unwrap_err(),INVALID);
        let dir=f.0.join("data/reference-timeline-temp");
        if dir.exists() {assert_eq!(std::fs::read_dir(dir).unwrap().count(),0);}
    }
    #[test]
    fn previously_rejected_h264_b_frames_have_a_trusted_fast_timeline() {
        let bytes=include_bytes!("../../tests/fixtures/reference-one-frame-offset.mp4");
        // PyAV/libx264: 72 decodable frames at 24fps; mdhd=37376,
        // stts=36864 at 12288Hz. Independent decoded span is exactly 3s.
        assert_eq!(duration_ms(bytes).unwrap(),3042);
        let report=timeline(bytes).unwrap();
        assert_eq!((report.video_samples,report.video_ticks,report.video_scale),(72,36864,12288));
        assert!(!report.deep);
    }
    #[test]
    fn small_difference_does_not_bypass_sample_range_or_sample_count_validation() {
        let f=super::super::video_frames::tests::Fixture::new();
        let original=std::fs::read(f.clip()).unwrap();
        for atom in [b"stco",b"stsz",b"stsc"] {
            let mut bytes=original.clone();change_video_header(&mut bytes,18432);
            let at=bytes.windows(4).position(|b|b==atom).unwrap()+4;
            let field=if atom==b"stsc" {12} else {8};
            bytes[at+field..at+field+4].copy_from_slice(&u32::MAX.to_be_bytes());
            assert_eq!(duration_ms(&bytes).unwrap_err(),INVALID);
        }
    }
    #[test]
    fn deep_verification_requires_a_digest_pinned_decoder() {
        let f=super::super::video_frames::tests::Fixture::new();
        let mut bytes=std::fs::read(f.clip()).unwrap();change_video_header(&mut bytes,24576);
        let asset=super::super::assets::create(&f.0,"owner","ref.mp4",Some("video/mp4"),&bytes).unwrap();
        let body=serde_json::json!({"video_asset_ids":[asset.id]});
        std::fs::write(f.0.join("video-frame-extractor.json"),serde_json::to_vec(&serde_json::json!({"path":f.1,"sha256":"0".repeat(64)})).unwrap()).unwrap();
        assert_eq!(owned_reference_seconds(&f.0,"owner",&body).unwrap_err(),"reference_video_verification_unavailable");
    }
    #[test]
    fn conservative_reference_limit_and_sum_remain_enforced() {
        let f=super::super::video_frames::tests::Fixture::new();
        let mut bytes=std::fs::read(f.clip()).unwrap();change_video_header(&mut bytes,18432);
        let a=super::super::assets::create(&f.0,"owner","a.mp4",Some("video/mp4"),&bytes).unwrap();
        let b=super::super::assets::create(&f.0,"owner","b.mp4",Some("video/mp4"),&bytes).unwrap();
        assert_eq!(owned_reference_seconds(&f.0,"owner",&serde_json::json!({"video_asset_ids":[a.id,b.id]})).unwrap(),Some(3));
        change_video_header(&mut bytes,999424); // 61s at 16384Hz.
        assert_eq!(timeline(&bytes).unwrap_err(),INVALID);
    }

    #[test]
    #[ignore="explicit local generated-media metadata probe; no upstream request"]
    fn generated_acceptance_mp4_metadata_probe() {
        assert_eq!(std::env::var("BRIDGE_MEDIA_PROBE_ACK").as_deref(),Ok("1"));
        let path=std::env::var("BRIDGE_MEDIA_PROBE_PATH").unwrap();
        assert!(std::path::Path::new(&path).is_absolute());
        let bytes=std::fs::read(path).unwrap();
        // Includes AAC priming: sample table is 5.120s, edited playback 5.088s.
        assert_eq!(duration_ms(&bytes).unwrap(),5120);
    }
}
