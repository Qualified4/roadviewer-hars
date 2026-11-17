"""Measurement-only copy of a HARS tree (argv[1]), never the product tree: timing scopes
(HARS_PROFILE_FILE), the scaled candidates and the levelled picture (HARS_THUMB_DUMP), MD5s of
the output frames (HARS_THUMB_FRAMES) and a whole-window decode (HARS_THUMB_ALLFRAMES)."""
import sys
from pathlib import Path
root = Path(sys.argv[1])

def sub(path, old, new, count=1):
    p = root / path
    s = p.read_text()
    assert s.count(old) == count, (path, old, s.count(old))
    p.write_text(s.replace(old, new))

S = 'crate::profile::Scope::new'
# thumbnail.rs
sub('src/thumbnail.rs',
    'first = Some(scale::small(&frame.image, WIDTH, frame.full_range)?);',
    'first = Some({ let _s = %s("thumb_scale"); scale::small(&frame.image, WIDTH, frame.full_range)? });' % S)
sub('src/thumbnail.rs',
    'candidates.push((second, scale::small(&frame.image, WIDTH, frame.full_range)?));',
    'candidates.push((second, { let _s = %s("thumb_scale"); scale::small(&frame.image, WIDTH, frame.full_range)? }));' % S)
sub('src/thumbnail.rs', '    let index = candidates\n', '    let score_scope = %s("thumb_score");\n    let index = candidates\n' % S)
sub('src/thumbnail.rs', '    let (second, mut image) = candidates.swap_remove(index);\n',
    '    drop(score_scope);\n    dump(output, "cand", &candidates);\n    let (second, mut image) = candidates.swap_remove(index);\n')
sub('src/thumbnail.rs', '    level(&mut image, measured);\n',
    '    { let _s = %s("thumb_level"); level(&mut image, measured); }\n    dump(output, "levelled", &[(second, image.clone())]);\n' % S)
sub('src/thumbnail.rs', '    let jpeg = jpeg::encode(&image)?;\n',
    '    let jpeg = { let _s = %s("thumb_jpeg"); jpeg::encode(&image)? };\n    let write_scope = %s("thumb_write");\n' % (S, S))
sub('src/thumbnail.rs', '    Ok(json!({"second":second,"score":meta["score"]}))\n',
    '    drop(write_scope);\n    Ok(json!({"second":second,"score":meta["score"]}))\n')
sub('src/thumbnail.rs', 'pub fn make(',
    '''fn dump(output: &Path, name: &str, images: &[(u8, Image)]) {
    if std::env::var_os("HARS_THUMB_DUMP").is_none() { return; }
    let mut bytes = Vec::new();
    for (s, i) in images { bytes.push(*s); for p in [&i.y, &i.u, &i.v] { bytes.extend((p.width as u32).to_le_bytes()); bytes.extend((p.height as u32).to_le_bytes()); bytes.extend(&p.data); } }
    let _ = std::fs::write(output.with_extension(name), bytes);
}
pub fn make(''')
# video.rs
sub('src/thumbnail/video.rs', '    let track = track(&mut file, limit)?;\n',
    '    let track = { let _s = %s("thumb_mp4"); track(&mut file, limit)? };\n' % S)
sub('src/thumbnail/video.rs', '                match guarded(|| d.decode(&au))? {\n',
    '                let decoded = { let _s = %s("thumb_decode"); guarded(|| d.decode(&au))? };\n                let _s = %s("thumb_image");\n                match decoded {\n' % (S, S))
sub('src/thumbnail/video.rs', '''                for (i, nal) in nals.iter().enumerate() {
                    let _ = guarded(|| d.push_nal(nal, (i == 0).then_some(pts)))?;
                }
                while let Ok(frame) = guarded(|| d.next_frame())? {
                    out.push((frame.pts.unwrap_or(pts), h265_image(&frame)?));
                }''', '''                let _s = %s("thumb_decode");
                for (i, nal) in nals.iter().enumerate() {
                    let _ = guarded(|| d.push_nal(nal, (i == 0).then_some(pts)))?;
                }
                while let Ok(frame) = guarded(|| d.next_frame())? {
                    let _i = %s("thumb_image");
                    out.push((frame.pts.unwrap_or(pts), h265_image(&frame)?));
                }''' % (S, S))
sub('src/thumbnail/video.rs', '        if file.read_exact(&mut sample).is_err() {\n',
    '        let read = { let _s = %s("thumb_read"); file.read_exact(&mut sample) };\n        if read.is_err() {\n' % S)
new_api = 'Option<f64>' in (root / 'src/thumbnail/video.rs').read_text()
keep_going = 'Some(f64::NEG_INFINITY)' if new_api else 'false'
sub('src/thumbnail.rs', '        let t = frame.time;\n',
    '        let t = frame.time;\n        frame_md5(&frame, "HARS_THUMB_FRAMES");\n')
sub('src/thumbnail.rs', 'pub fn make(source: &Path, output: &Path) -> anyhow::Result<Value> {\n',
    '''fn frame_md5(frame: &video::Frame, var: &str) {
    use md5::Digest;
    let Some(path) = std::env::var_os(var) else { return; };
    let mut h = md5::Md5::new();
    for p in [&frame.image.y, &frame.image.u, &frame.image.v] { h.update(&p.data); }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(f, "{:.6} {}x{} {}", frame.time, frame.image.y.width, frame.image.y.height, hex::encode(h.finalize())).unwrap();
}
pub fn make(source: &Path, output: &Path) -> anyhow::Result<Value> {
    if std::env::var_os("HARS_THUMB_ALLFRAMES").is_some() {
        video::decode(source, MAX_PACKETS, |frame| { frame_md5(&frame, "HARS_THUMB_ALLFRAMES"); Ok(%s) })?;
        anyhow::bail!("all frames written");
    }
''' % keep_going)
early_flush = '                    guarded(|| d.flush())?;\n'
if early_flush in (root / 'src/thumbnail/video.rs').read_text():
    sub('src/thumbnail/video.rs', early_flush,
        '                    let _f = %s("thumb_decode");\n' % S + early_flush)
sub('src/main.rs', '        roadviewer_hars::pure_remux::limit_memory(roadviewer_hars::thumbnail::MEMORY_BUDGET_MIB)?;\n',
    '        { let _s = roadviewer_hars::profile::Scope::new("thumb_limit_memory"); roadviewer_hars::pure_remux::limit_memory(roadviewer_hars::thumbnail::MEMORY_BUDGET_MIB)?; }\n')
print('instrumented', root)
