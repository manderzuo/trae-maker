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
                total=total.checked_add(duration_ms(&bytes)?).ok_or(INVALID)?;
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
pub(super) fn duration_ms(bytes: &[u8]) -> Result<u64, String> {
    if bytes.len()>super::assets::MAX_ASSET_BYTES {return Err(INVALID.into());}
    let top=boxes(bytes)?;
    one(&top,b"ftyp")?; one(&top,b"mdat")?;
    if top.iter().any(|(k,_)|*k==b"moof") {return Err(INVALID.into());}
    let movie=boxes(one(&top,b"moov")?)?;
    if movie.iter().any(|(k,_)|*k==b"mvex") {return Err(INVALID.into());}
    let mut video_count=0;let mut total=0;
    for (_,track) in movie.iter().filter(|(k,_)|*k==b"trak") {
        let track=boxes(track)?;
        // Edit lists can repeat/retime samples; do not infer price from an
        // unverified presentation timeline. This profile is non-fragmented MP4.
        if track.iter().any(|(k,_)|*k==b"edts") {return Err(INVALID.into());}
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
        let mut measured=0u64;let mut samples=0u64;
        for n in 0..entries {
            let count=u64::from(u32_at(timing,8+n*8)?);let delta=u64::from(u32_at(timing,12+n*8)?);
            if count==0 || delta==0 {return Err(INVALID.into());}
            samples=samples.checked_add(count).ok_or(INVALID)?;
            measured=measured.checked_add(count.checked_mul(delta).ok_or(INVALID)?).ok_or(INVALID)?;
        }
        if samples>1_000_000 || measured!=ticks {return Err(INVALID.into());}
        let ms=ticks.checked_mul(1000).and_then(|v|v.checked_add(scale-1)).ok_or(INVALID)?/scale;
        if ms==0 || ms>60_000 {return Err(INVALID.into());} total=total.max(ms);
    }
    if video_count!=1 {return Err(INVALID.into());} Ok(total)
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
}
